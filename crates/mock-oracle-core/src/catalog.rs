//! In-memory storage: tables, rows and the constraints that guard them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::ast::Expr;
use crate::value::key_string;
use crate::{SqlType, Value};

#[derive(Debug, Clone)]
pub struct TableColumn {
    pub name: String,
    pub sql_type: SqlType,
    pub default: Option<Expr>,
    pub not_null: bool,
    pub identity: bool,
}

#[derive(Debug, Clone)]
pub enum ConstraintKind {
    /// PRIMARY KEY, UNIQUE, or a unique index, with an index from key to row id.
    Unique {
        columns: Vec<usize>,
        primary: bool,
        index: HashMap<String, u64>,
    },
    Check(Expr),
    /// A non-unique index. It has no effect but can be dropped by name.
    Index,
}

#[derive(Debug, Clone)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    /// Created by CREATE INDEX rather than as a table constraint.
    pub is_index: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<TableColumn>,
    pub constraints: Vec<Constraint>,
    /// Rows by row id. Ids only grow, so this is insertion order.
    pub rows: BTreeMap<u64, Vec<Value>>,
}

/// The key a unique constraint indexes a row under, or `None` when every key column is NULL
/// (such rows never conflict).
fn unique_key(columns: &[usize], values: &[Value]) -> Option<String> {
    let key: Vec<&Value> = columns.iter().map(|&c| &values[c]).collect();
    if key.iter().all(|v| v.is_null()) {
        None
    } else {
        Some(key_string(key))
    }
}

impl Table {
    pub fn new(name: String, columns: Vec<TableColumn>) -> Self {
        Self {
            name,
            columns,
            constraints: Vec::new(),
            rows: BTreeMap::new(),
        }
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Inserts a row, or returns the name of the unique constraint it violates.
    pub fn insert(&mut self, id: u64, values: Vec<Value>) -> Result<(), String> {
        for c in &self.constraints {
            if let ConstraintKind::Unique { columns, index, .. } = &c.kind {
                if unique_key(columns, &values).is_some_and(|k| index.contains_key(&k)) {
                    return Err(c.name.clone());
                }
            }
        }
        for c in &mut self.constraints {
            if let ConstraintKind::Unique { columns, index, .. } = &mut c.kind {
                if let Some(k) = unique_key(columns, &values) {
                    index.insert(k, id);
                }
            }
        }
        self.rows.insert(id, values);
        Ok(())
    }

    pub fn delete(&mut self, id: u64) -> bool {
        let Some(values) = self.rows.remove(&id) else {
            return false;
        };
        self.unindex(&values);
        true
    }

    fn unindex(&mut self, values: &[Value]) {
        for c in &mut self.constraints {
            if let ConstraintKind::Unique { columns, index, .. } = &mut c.kind {
                if let Some(k) = unique_key(columns, values) {
                    index.remove(&k);
                }
            }
        }
    }

    /// Replaces the values of several rows at once, so that for example `SET id = id + 1`
    /// does not trip over its own intermediate state. Rows that no longer exist are skipped.
    /// On a unique violation the table is left partly updated; callers discard it.
    pub fn update(&mut self, changes: Vec<(u64, Vec<Value>)>) -> Result<(), String> {
        let changes: Vec<_> = changes
            .into_iter()
            .filter(|(id, _)| self.rows.contains_key(id))
            .collect();
        for (id, _) in &changes {
            let old = self.rows[id].clone();
            self.unindex(&old);
        }
        for (id, values) in changes {
            for c in &mut self.constraints {
                if let ConstraintKind::Unique { columns, index, .. } = &mut c.kind {
                    if let Some(k) = unique_key(columns, &values) {
                        if index.insert(k, id).is_some_and(|other| other != id) {
                            return Err(c.name.clone());
                        }
                    }
                }
            }
            self.rows.insert(id, values);
        }
        Ok(())
    }

    pub fn truncate(&mut self) {
        self.rows.clear();
        for c in &mut self.constraints {
            if let ConstraintKind::Unique { index, .. } = &mut c.kind {
                index.clear();
            }
        }
    }

    /// Adds a unique constraint over existing rows, failing if they already hold duplicates.
    pub fn add_unique(
        &mut self,
        name: String,
        columns: Vec<usize>,
        primary: bool,
        is_index: bool,
    ) -> Result<(), ()> {
        let mut index = HashMap::new();
        for (id, values) in &self.rows {
            if let Some(k) = unique_key(&columns, values) {
                if index.insert(k, *id).is_some() {
                    return Err(());
                }
            }
        }
        self.constraints.push(Constraint {
            name,
            kind: ConstraintKind::Unique {
                columns,
                primary,
                index,
            },
            is_index,
        });
        Ok(())
    }

    pub fn has_primary_key(&self) -> bool {
        self.constraints
            .iter()
            .any(|c| matches!(c.kind, ConstraintKind::Unique { primary: true, .. }))
    }
}

/// All tables. Cloning is cheap: tables are shared until written (copy on write).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub tables: BTreeMap<String, Arc<Table>>,
    /// Bumped on every commit and DDL, so a transaction can tell whether it is still current.
    pub version: u64,
}

impl Catalog {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(name).map(|t| t.as_ref())
    }

    pub fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(name).map(Arc::make_mut)
    }

    /// Whether a table, constraint or index already uses `name`.
    pub fn name_in_use(&self, name: &str) -> bool {
        self.tables.contains_key(name)
            || self
                .tables
                .values()
                .any(|t| t.constraints.iter().any(|c| c.name == name))
    }
}
