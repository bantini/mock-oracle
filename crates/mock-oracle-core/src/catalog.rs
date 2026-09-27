//! In-memory storage: tables, rows and the constraints that guard them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::ast::{Expr, OnDelete};
use crate::plsql::Routine;
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
    /// A foreign key: `columns` of this table reference `parent_columns` of `parent`,
    /// which match the columns of a primary key or unique constraint there.
    ForeignKey {
        columns: Vec<usize>,
        parent: String,
        parent_columns: Vec<usize>,
        on_delete: OnDelete,
    },
    /// A non-unique index. It has no effect but can be dropped by name.
    Index,
}

#[derive(Debug, Clone)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    /// Created by CREATE INDEX rather than as a table constraint.
    pub is_index: bool,
    /// A disabled constraint is kept but not enforced; a disabled unique constraint
    /// keeps an empty index.
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<TableColumn>,
    pub constraints: Vec<Constraint>,
    /// Rows by row id. Ids only grow, so this is insertion order.
    pub rows: BTreeMap<u64, Vec<Value>>,
}

/// The key a foreign key looks up in its parent, or `None` when any key column is NULL
/// (such rows are not checked).
pub fn foreign_key(columns: &[usize], values: &[Value]) -> Option<String> {
    let key: Vec<&Value> = columns.iter().map(|&c| &values[c]).collect();
    if key.iter().any(|v| v.is_null()) {
        None
    } else {
        Some(key_string(key))
    }
}

/// The key a unique constraint indexes a row under, or `None` when every key column is NULL
/// (such rows never conflict).
pub fn unique_key(columns: &[usize], values: &[Value]) -> Option<String> {
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
            if let (ConstraintKind::Unique { columns, index, .. }, true) = (&c.kind, c.enabled) {
                if unique_key(columns, &values).is_some_and(|k| index.contains_key(&k)) {
                    return Err(c.name.clone());
                }
            }
        }
        for c in &mut self.constraints {
            if let (ConstraintKind::Unique { columns, index, .. }, true) = (&mut c.kind, c.enabled)
            {
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
            if let (ConstraintKind::Unique { columns, index, .. }, true) = (&mut c.kind, c.enabled)
            {
                if let Some(k) = unique_key(columns, values) {
                    index.remove(&k);
                }
            }
        }
    }

    /// Replaces the values of several rows at once, so that for example `SET id = id + 1`
    /// does not trip over its own intermediate state. Rows that no longer exist are skipped.
    /// On a unique violation nothing changes.
    pub fn update(&mut self, changes: Vec<(u64, Vec<Value>)>) -> Result<(), String> {
        let changes: Vec<_> = changes
            .into_iter()
            .filter(|(id, _)| self.rows.contains_key(id))
            .collect();
        let olds: Vec<Vec<Value>> = changes
            .iter()
            .map(|(id, _)| self.rows[id].clone())
            .collect();
        for old in &olds {
            self.unindex(old);
        }
        // Check every new key before changing anything, so a violation leaves the table as it was.
        let mut conflict = None;
        'check: for c in &self.constraints {
            if let (ConstraintKind::Unique { columns, index, .. }, true) = (&c.kind, c.enabled) {
                let mut seen = std::collections::HashSet::new();
                for (_, values) in &changes {
                    if let Some(k) = unique_key(columns, values) {
                        if index.contains_key(&k) || !seen.insert(k) {
                            conflict = Some(c.name.clone());
                            break 'check;
                        }
                    }
                }
            }
        }
        let rows = if conflict.is_some() {
            changes.iter().map(|(id, _)| *id).zip(olds).collect()
        } else {
            changes
        };
        for (id, values) in rows {
            for c in &mut self.constraints {
                if let (ConstraintKind::Unique { columns, index, .. }, true) =
                    (&mut c.kind, c.enabled)
                {
                    if let Some(k) = unique_key(columns, &values) {
                        index.insert(k, id);
                    }
                }
            }
            self.rows.insert(id, values);
        }
        conflict.map_or(Ok(()), Err)
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
    /// A disabled one starts with an empty index.
    pub fn add_unique(
        &mut self,
        name: String,
        columns: Vec<usize>,
        primary: bool,
        is_index: bool,
        enabled: bool,
    ) -> Result<(), ()> {
        let index = if enabled {
            self.build_index(&columns)?
        } else {
            HashMap::new()
        };
        self.constraints.push(Constraint {
            name,
            kind: ConstraintKind::Unique {
                columns,
                primary,
                index,
            },
            is_index,
            enabled,
        });
        Ok(())
    }

    fn build_index(&self, columns: &[usize]) -> Result<HashMap<String, u64>, ()> {
        let mut index = HashMap::new();
        for (id, values) in &self.rows {
            if let Some(k) = unique_key(columns, values) {
                if index.insert(k, *id).is_some() {
                    return Err(());
                }
            }
        }
        Ok(index)
    }

    /// Turns the constraint at position `i` on or off. Enabling a unique constraint
    /// rebuilds its index and fails if the rows hold duplicates; disabling empties it.
    pub fn set_enabled(&mut self, i: usize, enabled: bool) -> Result<(), ()> {
        let rebuilt = match &self.constraints[i].kind {
            ConstraintKind::Unique { columns, .. } if enabled => Some(self.build_index(columns)?),
            ConstraintKind::Unique { .. } => Some(HashMap::new()),
            _ => None,
        };
        let c = &mut self.constraints[i];
        if let (ConstraintKind::Unique { index, .. }, Some(rebuilt)) = (&mut c.kind, rebuilt) {
            *index = rebuilt;
        }
        c.enabled = enabled;
        Ok(())
    }

    /// Whether some row has these values in `columns`: through a unique index over
    /// exactly those columns when there is one, otherwise by scanning.
    pub fn has_key(&self, columns: &[usize], key: &str) -> bool {
        for c in &self.constraints {
            if let (
                ConstraintKind::Unique {
                    columns: cols,
                    index,
                    ..
                },
                true,
            ) = (&c.kind, c.enabled)
            {
                if cols == columns {
                    return index.contains_key(key);
                }
            }
        }
        self.rows
            .values()
            .any(|r| unique_key(columns, r).as_deref() == Some(key))
    }

    pub fn has_primary_key(&self) -> bool {
        self.constraints
            .iter()
            .any(|c| matches!(c.kind, ConstraintKind::Unique { primary: true, .. }))
    }
}

/// A sequence's definition. Its counter lives in the [`crate::Database`], outside
/// transactions.
#[derive(Debug, Clone, PartialEq)]
pub struct SequenceDef {
    pub start: i128,
    pub increment: i128,
    pub min: i128,
    pub max: i128,
    pub cycle: bool,
}

/// A foreign key found by [`Catalog::foreign_keys`].
#[derive(Debug, Clone)]
pub struct ForeignKeyRef {
    pub child: String,
    pub name: String,
    pub columns: Vec<usize>,
    pub parent: String,
    pub parent_columns: Vec<usize>,
    pub on_delete: OnDelete,
    pub enabled: bool,
}

/// All tables, sequences and stored routines. Cloning is cheap: tables are shared until written (copy on write).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub tables: BTreeMap<String, Arc<Table>>,
    pub sequences: BTreeMap<String, SequenceDef>,
    /// Stored procedures and functions.
    pub routines: BTreeMap<String, Arc<Routine>>,
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

    /// Every foreign key in the catalog.
    pub fn foreign_keys(&self) -> impl Iterator<Item = ForeignKeyRef> + '_ {
        self.tables.values().flat_map(|t| {
            t.constraints.iter().filter_map(|c| match &c.kind {
                ConstraintKind::ForeignKey {
                    columns,
                    parent,
                    parent_columns,
                    on_delete,
                } => Some(ForeignKeyRef {
                    child: t.name.clone(),
                    name: c.name.clone(),
                    columns: columns.clone(),
                    parent: parent.clone(),
                    parent_columns: parent_columns.clone(),
                    on_delete: *on_delete,
                    enabled: c.enabled,
                }),
                _ => None,
            })
        })
    }

    /// The foreign keys that reference the key over `columns` of `table`.
    pub fn referencing(&self, table: &str, columns: &[usize]) -> Vec<ForeignKeyRef> {
        self.foreign_keys()
            .filter(|f| f.parent == table && f.parent_columns == columns)
            .collect()
    }

    /// Whether a table, sequence, routine, constraint or index already uses `name`.
    pub fn name_in_use(&self, name: &str) -> bool {
        self.tables.contains_key(name)
            || self.sequences.contains_key(name)
            || self.routines.contains_key(name)
            || self
                .tables
                .values()
                .any(|t| t.constraints.iter().any(|c| c.name == name))
    }
}
