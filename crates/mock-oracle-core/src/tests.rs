//! End-to-end tests of the engine through the public API.

use std::sync::Arc;

use chrono::NaiveDate;

use crate::*;

fn db() -> Arc<Database> {
    Arc::new(Database::new())
}

fn n(s: &str) -> Value {
    Value::parse_number(s).unwrap()
}

fn s(x: &str) -> Value {
    Value::varchar(x)
}

/// Runs a query and returns its rows.
fn rows(db: &Arc<Database>, sql: &str) -> Vec<Vec<Value>> {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
}

/// Runs a query that returns one value.
fn one(db: &Arc<Database>, sql: &str) -> Value {
    let r = rows(db, sql);
    assert_eq!(r.len(), 1, "{sql}");
    r[0][0].clone()
}

fn err(db: &Arc<Database>, sql: &str) -> u32 {
    db.execute(sql, &[])
        .map(|_| panic!("{sql} should fail"))
        .unwrap_err()
        .code
}

fn emp() -> Arc<Database> {
    let db = db();
    db.run_script(
        "
        create table dept (id number(4) primary key, name varchar2(30) not null unique);
        create table emp (
            id number(6) constraint emp_pk primary key,
            name varchar2(40) not null,
            dept_id number(4) references dept(id),
            salary number(8,2) check (salary > 0),
            hired date,
            manager_id number(6)
        );
        insert into dept values (10, 'Sales');
        insert into dept values (20, 'Engineering');
        insert into dept values (30, 'Empty');
        insert into emp values (1, 'Ada', 20, 9000, date '2020-01-15', null);
        insert into emp values (2, 'Bob', 10, 5000, date '2021-06-01', 1);
        insert into emp values (3, 'Cy', 20, 7000.5, date '2019-03-10', 1);
        insert into emp values (4, 'Dee', null, 3000, null, 2);
        ",
    )
    .unwrap();
    db
}

#[test]
fn select_from_dual() {
    let db = db();
    let r = db.execute("SELECT 1 FROM DUAL", &[]).unwrap();
    assert!(r.is_query);
    assert_eq!(
        r.columns,
        vec![Column {
            name: "1".into(),
            sql_type: SqlType::NUMBER
        }]
    );
    assert_eq!(r.rows, vec![vec![n("1")]]);
    let r = db
        .execute(
            "select 2 * (3 + 4) n, 'a' || 1 || null s, '' e, dummy from dual",
            &[],
        )
        .unwrap();
    assert_eq!(r.rows[0], vec![n("14"), s("a1"), Value::Null, s("X")]);
    assert_eq!(r.columns[1].sql_type, SqlType::Varchar2(2));
    assert_eq!(r.columns[2].sql_type, SqlType::Varchar2(0));
    assert_eq!(rows(&db, "select * from dual"), vec![vec![s("X")]]);
}

#[test]
fn binds() {
    let db = db();
    let r = db
        .execute("select :a + 1, :b from dual", &[s("41"), s("hi")])
        .unwrap();
    assert_eq!(r.rows[0], vec![n("42"), s("hi")]);
    assert_eq!(
        db.execute("select :a from dual", &[]).unwrap_err().code,
        1008
    );
}

#[test]
fn oracle_errors() {
    let db = db();
    assert_eq!(
        db.execute("select 1/0 from dual", &[])
            .unwrap_err()
            .to_string(),
        "ORA-01476: divisor is equal to zero"
    );
    assert_eq!(err(&db, "select 'x' + 1 from dual"), 1722);
    assert_eq!(
        db.execute("select foo from dual", &[])
            .unwrap_err()
            .to_string(),
        "ORA-00904: \"FOO\": invalid identifier"
    );
    assert_eq!(err(&db, "FROB"), 900);
    assert_eq!(err(&db, "select * from nope"), 942);
}

#[test]
fn create_insert_select() {
    let db = emp();
    let r = db.execute("select * from emp where id = 1", &[]).unwrap();
    let names: Vec<_> = r.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["ID", "NAME", "DEPT_ID", "SALARY", "HIRED", "MANAGER_ID"]
    );
    assert_eq!(
        r.columns[0].sql_type,
        SqlType::Number {
            precision: 6,
            scale: 0
        }
    );
    assert_eq!(r.columns[1].sql_type, SqlType::Varchar2(40));
    assert_eq!(r.columns[4].sql_type, SqlType::Date);
    let hired = NaiveDate::from_ymd_opt(2020, 1, 15)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            n("1"),
            s("Ada"),
            n("20"),
            n("9000"),
            Value::Date(hired),
            Value::Null
        ]]
    );
    assert_eq!(one(&db, "select count(*) from emp"), n("4"));
    // Columns keep insertion order and scale rounding.
    assert_eq!(
        one(&db, "select salary from emp where name = 'Cy'"),
        n("7000.5")
    );
}

#[test]
fn constraints() {
    let db = emp();
    let e = db
        .execute("insert into emp (id, name) values (1, 'Dup')", &[])
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "ORA-00001: unique constraint (MOCK.EMP_PK) violated"
    );
    assert_eq!(err(&db, "insert into dept values (40, 'Sales')"), 1);
    let e = db
        .execute("insert into emp (id) values (9)", &[])
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "ORA-01400: cannot insert NULL into (\"MOCK\".\"EMP\".\"NAME\")"
    );
    let e = db
        .execute("update emp set name = null where id = 1", &[])
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "ORA-01407: cannot update (\"MOCK\".\"EMP\".\"NAME\") to NULL"
    );
    assert_eq!(
        err(
            &db,
            "insert into emp (id, name, salary) values (9, 'X', -1)"
        ),
        2290
    );
    assert_eq!(
        err(&db, "insert into emp (id, name) values (1234567, 'X')"),
        1438
    );
    let e = db
        .execute("insert into dept values (41, rpad('x', 31, 'x'))", &[])
        .unwrap_err();
    assert_eq!(e.to_string(), "ORA-12899: value too large for column \"MOCK\".\"DEPT\".\"NAME\" (actual: 31, maximum: 30)");
    assert_eq!(err(&db, "insert into dept values (41)"), 947);
    assert_eq!(err(&db, "insert into dept values (41, 'a', 'b')"), 913);
    assert_eq!(
        err(&db, "insert into dept (id, nope) values (41, 'a')"),
        904
    );
    assert_eq!(
        err(
            &db,
            "insert into emp (id, name, salary) values (9, 'X', 'abc')"
        ),
        1722
    );
    assert_eq!(err(&db, "create table dept (x number)"), 955);
    // A failed statement leaves earlier rows alone, and UPDATE sees the statement as a whole.
    assert_eq!(one(&db, "select count(*) from emp"), n("4"));
    assert_eq!(
        db.execute("update emp set id = id + 1", &[])
            .unwrap()
            .rows_affected,
        4
    );
    assert_eq!(err(&db, "update emp set id = 3 where id = 2"), 1);
    // Numbers are rounded to the column's scale.
    db.execute("update emp set salary = 1.005 where id = 2", &[])
        .unwrap();
    assert_eq!(one(&db, "select salary from emp where id = 2"), n("1.01"));
    // CHAR pads, and compares with unpadded literals.
    db.execute("create table c (code char(3))", &[]).unwrap();
    db.execute("insert into c values ('ab')", &[]).unwrap();
    assert_eq!(
        one(&db, "select code || '|' from c where code = 'ab'"),
        s("ab |")
    );
}

#[test]
fn update_delete_counts() {
    let db = emp();
    assert_eq!(
        db.execute("update emp set salary = salary * 2 where dept_id = 20", &[])
            .unwrap()
            .rows_affected,
        2
    );
    assert_eq!(
        one(&db, "select sum(salary) from emp"),
        n(&(18000.0 + 14001.0 + 5000.0 + 3000.0).to_string())
    );
    assert_eq!(
        db.execute("delete from emp where salary < 6000", &[])
            .unwrap()
            .rows_affected,
        2
    );
    assert_eq!(
        db.execute("delete from emp where 1 = 0", &[])
            .unwrap()
            .rows_affected,
        0
    );
    assert_eq!(
        db.execute("truncate table emp", &[]).unwrap().rows_affected,
        0
    );
    assert_eq!(one(&db, "select count(*) from emp"), n("0"));
    db.execute("drop table emp", &[]).unwrap();
    assert_eq!(err(&db, "select * from emp"), 942);
    assert_eq!(err(&db, "drop table emp"), 942);
}

#[test]
fn where_and_order() {
    let db = emp();
    let names =
        |sql: &str| -> Vec<Value> { rows(&db, sql).into_iter().map(|r| r[0].clone()).collect() };
    assert_eq!(
        names("select name from emp order by salary desc"),
        [s("Ada"), s("Cy"), s("Bob"), s("Dee")]
    );
    assert_eq!(
        names("select name from emp order by dept_id, name desc"),
        [s("Bob"), s("Cy"), s("Ada"), s("Dee")]
    );
    assert_eq!(
        names("select name from emp order by dept_id desc"),
        [s("Dee"), s("Ada"), s("Cy"), s("Bob")]
    );
    assert_eq!(
        names("select name from emp order by dept_id nulls first, 1"),
        [s("Dee"), s("Bob"), s("Ada"), s("Cy")]
    );
    assert_eq!(
        names("select name n from emp order by n"),
        [s("Ada"), s("Bob"), s("Cy"), s("Dee")]
    );
    assert_eq!(
        names("select name from emp where dept_id is null"),
        [s("Dee")]
    );
    assert_eq!(
        names(
            "select name from emp where name like '_e%' or salary between 6000 and 7500 order by 1"
        ),
        [s("Cy"), s("Dee")]
    );
    assert_eq!(
        names("select name from emp where id in (2, 4, null) order by id"),
        [s("Bob"), s("Dee")]
    );
    assert_eq!(
        names("select name from emp where id not in (2, null)"),
        Vec::<Value>::new()
    );
    assert_eq!(
        names("select name from emp where dept_id <> 20"),
        [s("Bob")]
    );
    assert_eq!(
        names("select name from emp where hired > '01-JAN-21'"),
        [s("Bob")]
    );
    assert_eq!(
        names("select name from emp where hired < date '2020-01-01'"),
        [s("Cy")]
    );
    assert_eq!(
        names("select name from emp order by id offset 1 rows fetch next 2 rows only"),
        [s("Bob"), s("Cy")]
    );
    assert_eq!(
        names("select name from emp where rownum <= 2"),
        [s("Ada"), s("Bob")]
    );
    assert_eq!(
        names("select * from (select name from emp order by salary) where rownum = 1"),
        [s("Dee")]
    );
    assert_eq!(
        names("select distinct dept_id from emp order by 1"),
        [n("10"), n("20"), Value::Null]
    );
}

#[test]
fn joins_and_subqueries() {
    let db = emp();
    let r = rows(
        &db,
        "select e.name, d.name from emp e join dept d on d.id = e.dept_id order by e.id",
    );
    assert_eq!(
        r,
        vec![
            vec![s("Ada"), s("Engineering")],
            vec![s("Bob"), s("Sales")],
            vec![s("Cy"), s("Engineering")]
        ]
    );
    let r = rows(
        &db,
        "select e.name, d.name from emp e left join dept d on d.id = e.dept_id where d.id is null",
    );
    assert_eq!(r, vec![vec![s("Dee"), Value::Null]]);
    let r = rows(&db, "select d.name, count(e.id) from dept d left outer join emp e on e.dept_id = d.id group by d.name order by 1");
    assert_eq!(
        r,
        vec![
            vec![s("Empty"), n("0")],
            vec![s("Engineering"), n("2")],
            vec![s("Sales"), n("1")]
        ]
    );
    assert_eq!(
        rows(
            &db,
            "select count(*) from emp full join dept on emp.dept_id = dept.id"
        )[0][0],
        n("5")
    );
    assert_eq!(rows(&db, "select count(*) from emp, dept")[0][0], n("12"));
    assert_eq!(
        rows(
            &db,
            "select count(*) from emp e, dept d where e.dept_id = d.id"
        )[0][0],
        n("3")
    );
    let r = rows(
        &db,
        "select e.name, m.name from emp e join emp m on m.id = e.manager_id order by e.id",
    );
    assert_eq!(
        r,
        vec![
            vec![s("Bob"), s("Ada")],
            vec![s("Cy"), s("Ada")],
            vec![s("Dee"), s("Bob")]
        ]
    );
    assert_eq!(
        err(
            &db,
            "select name from emp e join dept d on d.id = e.dept_id"
        ),
        918
    );
    // Scalar, IN and correlated subqueries.
    assert_eq!(
        one(
            &db,
            "select name from emp where salary = (select max(salary) from emp)"
        ),
        s("Ada")
    );
    assert_eq!(
        one(
            &db,
            "select count(*) from emp where dept_id in (select id from dept where name like 'E%')"
        ),
        n("2")
    );
    let r = rows(
        &db,
        "select name from dept d where not exists (select 1 from emp e where e.dept_id = d.id)",
    );
    assert_eq!(r, vec![vec![s("Empty")]]);
    let r = rows(&db, "select name, (select count(*) from emp e where e.dept_id = d.id) c from dept d order by c desc, name");
    assert_eq!(r[0], vec![s("Engineering"), n("2")]);
    assert_eq!(err(&db, "select (select id from emp) from dual"), 1427);
    let r = rows(&db, "select name from emp e where salary > (select avg(salary) from emp x where x.dept_id = e.dept_id)");
    assert_eq!(r, vec![vec![s("Ada")]]);
}

#[test]
fn grouping_and_aggregates() {
    let db = emp();
    let r = rows(&db, "select dept_id, count(*), sum(salary), avg(salary), min(name), max(hired) from emp group by dept_id order by dept_id");
    assert_eq!(r[1][0..4], [n("20"), n("2"), n("16000.5"), n("8000.25")]);
    assert_eq!(r[2][0..3], [Value::Null, n("1"), n("3000")]);
    let r = rows(
        &db,
        "select dept_id from emp group by dept_id having count(*) > 1",
    );
    assert_eq!(r, vec![vec![n("20")]]);
    assert_eq!(
        rows(
            &db,
            "select count(*), sum(salary), max(id) from emp where 1 = 0"
        )[0],
        [n("0"), Value::Null, Value::Null]
    );
    assert_eq!(one(&db, "select count(distinct dept_id) from emp"), n("2"));
    assert_eq!(one(&db, "select count(dept_id) from emp"), n("3"));
    assert_eq!(err(&db, "select * from emp where count(*) > 1"), 934);
    assert_eq!(
        one(
            &db,
            "select avg(x) from (select 1 x from dual union all select 2 from dual)"
        ),
        n("1.5")
    );
    assert_eq!(one(&db, "select round(avg(x), 2) from (select 1 x from dual union all select 1 from dual union all select 2 from dual)"), n("1.33"));
}

#[test]
fn set_operations() {
    let db = emp();
    let r = rows(
        &db,
        "select dept_id from emp union select id from dept order by 1",
    );
    assert_eq!(r.len(), 4);
    assert_eq!(
        rows(&db, "select dept_id from emp union all select id from dept").len(),
        7
    );
    assert_eq!(
        rows(
            &db,
            "select dept_id from emp intersect select id from dept order by 1"
        ),
        vec![vec![n("10")], vec![n("20")]]
    );
    assert_eq!(
        rows(&db, "select id from dept minus select dept_id from emp"),
        vec![vec![n("30")]]
    );
    assert_eq!(
        err(&db, "select 1, 2 from dual union select 1 from dual"),
        1789
    );
    assert_eq!(
        err(&db, "select 1 from dual union select 'a' from dual"),
        1790
    );
}

#[test]
fn functions() {
    let db = db();
    let cases = [
        ("nvl(null, 'x')", s("x")),
        ("nvl2(1, 'a', 'b')", s("a")),
        ("coalesce(null, null, 3)", n("3")),
        ("decode(2, 1, 'one', 2, 'two', 'other')", s("two")),
        ("decode(null, null, 'null')", s("null")),
        ("nullif(1, 1)", Value::Null),
        ("upper('abc') || lower('DEF') || initcap('hello wORLD')", s("ABCdefHello World")),
        ("length('héllo')", n("5")),
        ("substr('hello', 2, 3)", s("ell")),
        ("substr('hello', -3)", s("llo")),
        ("instr('hello', 'l')", n("3")),
        ("instr('hello', 'l', -1)", n("4")),
        ("replace('aXbX', 'X', '-')", s("a-b-")),
        ("trim('  x  ') || '|'", s("x|")),
        ("trim(leading '0' from '0012')", s("12")),
        ("ltrim('xxy', 'x')", s("y")),
        ("lpad('7', 3, '0')", s("007")),
        ("rpad('ab', 4, '*')", s("ab**")),
        ("round(2.555, 2)", n("2.56")),
        ("trunc(2.555, 1)", n("2.5")),
        ("round(1234, -2)", n("1200")),
        ("mod(10, 3)", n("1")),
        ("mod(-10, 3)", n("-1")),
        ("power(2, 10)", n("1024")),
        ("abs(-3) + ceil(1.2) + floor(1.8)", n("6")),
        ("greatest(1, 3, 2)", n("3")),
        ("least('b', 'a')", s("a")),
        ("to_char(1234.5, '9,999.99')", s(" 1,234.50")),
        ("to_char(42)", s("42")),
        ("to_number('12.50')", n("12.5")),
        ("to_char(to_date('2024-02-10 13:05:09', 'YYYY-MM-DD HH24:MI:SS'), 'DD/MM/YYYY HH:MI:SS AM')", s("10/02/2024 01:05:09 PM")),
        ("to_char(add_months(date '2024-01-31', 1), 'YYYY-MM-DD')", s("2024-02-29")),
        ("to_char(last_day(date '2023-02-10'), 'DD')", s("28")),
        ("months_between(date '2024-03-15', date '2024-01-15')", n("2")),
        ("date '2024-03-01' - date '2024-02-01'", n("29")),
        ("to_char(date '2024-03-01' + 1.5, 'YYYY-MM-DD HH24:MI')", s("2024-03-02 12:00")),
        ("to_char(trunc(to_date('2024-05-17 10:00', 'YYYY-MM-DD HH24:MI'), 'MM'), 'YYYY-MM-DD')", s("2024-05-01")),
        ("extract(year from date '2024-05-17')", n("2024")),
        ("cast('42' as number) + 1", n("43")),
        ("case when 1 = 2 then 'a' when 2 = 2 then 'b' end", s("b")),
        ("case 3 when 1 then 'a' else 'z' end", s("z")),
        ("chr(65) || ascii('a')", s("A97")),
        ("1/3", n(".3333333333333333333333333333333333333333")),
        ("user", s("MOCK")),
    ];
    for (expr, expected) in cases {
        assert_eq!(
            one(&db, &format!("select {expr} from dual")),
            expected,
            "{expr}"
        );
    }
    assert!(matches!(
        one(&db, "select sysdate from dual"),
        Value::Date(_)
    ));
    assert!(matches!(
        one(&db, "select systimestamp from dual"),
        Value::Timestamp(_)
    ));
    assert_eq!(err(&db, "select nosuch(1) from dual"), 904);
    assert_eq!(err(&db, "select nvl(1) from dual"), 909);
}

#[test]
fn nls_date_format() {
    let db = db();
    let mut s1 = db.session("scott");
    s1.execute("create table t (d date)", &[]).unwrap();
    s1.execute("insert into t values ('05-MAR-24')", &[])
        .unwrap();
    s1.execute("alter session set nls_date_format = 'YYYY-MM-DD'", &[])
        .unwrap();
    let r = s1
        .execute("select to_char(d), d || '' from t", &[])
        .unwrap();
    assert_eq!(r.rows[0], vec![s("2024-03-05"), s("2024-03-05")]);
    s1.execute("insert into t values ('2024-12-31')", &[])
        .unwrap();
    assert_eq!(
        s1.execute("select count(*) from t where d > '2024-06-01'", &[])
            .unwrap()
            .rows[0][0],
        n("1")
    );
    assert_eq!(
        s1.execute("insert into t values ('31/12/2024')", &[])
            .unwrap_err()
            .code,
        1830
    );
    assert_eq!(
        s1.execute("alter session set nls_date_format = 'QQQ'", &[])
            .unwrap_err()
            .code,
        1821
    );
    assert_eq!(
        s1.execute("select user from dual", &[]).unwrap().rows[0][0],
        s("SCOTT")
    );
}

#[test]
fn transactions() {
    let db = emp();
    let mut a = db.session("app");
    let mut b = db.session("app");
    a.execute("insert into dept values (40, 'Ops')", &[])
        .unwrap();
    assert!(a.in_transaction());
    let count =
        |s: &mut Session| s.execute("select count(*) from dept", &[]).unwrap().rows[0][0].clone();
    assert_eq!(count(&mut a), n("4"));
    assert_eq!(count(&mut b), n("3"), "uncommitted rows are private");
    a.rollback();
    assert_eq!(count(&mut a), n("3"));
    a.execute("insert into dept values (40, 'Ops')", &[])
        .unwrap();
    a.execute("commit", &[]).unwrap();
    assert!(!a.in_transaction());
    assert_eq!(count(&mut b), n("4"));

    // Concurrent transactions on different rows both commit.
    a.execute("update dept set name = 'Ops2' where id = 40", &[])
        .unwrap();
    b.execute("delete from dept where id = 30", &[]).unwrap();
    b.commit().unwrap();
    a.commit().unwrap();
    let r = db
        .execute("select id, name from dept order by id", &[])
        .unwrap()
        .rows;
    assert_eq!(
        r,
        vec![
            vec![n("10"), s("Sales")],
            vec![n("20"), s("Engineering")],
            vec![n("40"), s("Ops2")]
        ]
    );

    // Conflicting inserts: the second commit fails and is rolled back.
    a.execute("insert into dept values (50, 'X')", &[]).unwrap();
    b.execute("insert into dept values (50, 'Y')", &[]).unwrap();
    a.commit().unwrap();
    assert_eq!(b.commit().unwrap_err().code, 1);
    assert_eq!(
        db.execute("select name from dept where id = 50", &[])
            .unwrap()
            .rows,
        vec![vec![s("X")]]
    );

    // DDL commits the open transaction.
    a.execute("insert into dept values (60, 'Z')", &[]).unwrap();
    a.execute("create table other (x number)", &[]).unwrap();
    a.rollback();
    assert_eq!(count(&mut b), n("5"));

    // Dropping a session rolls back.
    {
        let mut c = db.session("app");
        c.execute("delete from dept", &[]).unwrap();
    }
    assert_eq!(count(&mut b), n("5"));
}

#[test]
fn snapshot_and_restore() {
    let db = emp();
    let snap = db.snapshot();
    db.execute("delete from emp", &[]).unwrap();
    db.execute("drop table dept", &[]).unwrap();
    db.restore(&snap);
    assert_eq!(one(&db, "select count(*) from emp"), n("4"));
    assert_eq!(one(&db, "select count(*) from dept"), n("3"));
    db.reset();
    assert_eq!(err(&db, "select * from emp"), 942);
}

#[test]
fn identity_default_and_ctas() {
    let db = db();
    db.run_script(
        "create table t (
            id number generated by default on null as identity,
            label varchar2(10) default 'none',
            created date default sysdate not null
         );
         insert into t (label) values ('a');
         insert into t (id, label) values (null, 'b');
         insert into t (id) values (100);
        ",
    )
    .unwrap();
    let r = rows(&db, "select id, label from t order by id");
    assert_eq!(
        r,
        vec![
            vec![n("1"), s("a")],
            vec![n("2"), s("b")],
            vec![n("100"), s("none")]
        ]
    );
    let snap = db.snapshot();
    db.execute("insert into t (label) values ('c')", &[])
        .unwrap();
    db.restore(&snap);
    db.execute("insert into t (label) values ('c')", &[])
        .unwrap();
    assert_eq!(one(&db, "select id from t where label = 'c'"), n("3"));

    db.execute(
        "create table t2 as select id, label, 'x' tag from t where id < 100",
        &[],
    )
    .unwrap();
    let r = db.execute("select * from t2 order by id", &[]).unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(
        r.columns[2],
        Column {
            name: "TAG".into(),
            sql_type: SqlType::Varchar2(1)
        }
    );
    db.execute("insert into t2 select id + 10, label, tag from t2", &[])
        .unwrap();
    assert_eq!(one(&db, "select count(*) from t2"), n("6"));
}

#[test]
fn unique_index() {
    let db = db();
    db.run_script("create table u (a number, b number); create unique index u_ab on u (a, b); insert into u values (1, 1)").unwrap();
    let e = db.execute("insert into u values (1, 1)", &[]).unwrap_err();
    assert_eq!(
        e.to_string(),
        "ORA-00001: unique constraint (MOCK.U_AB) violated"
    );
    db.execute("insert into u values (null, null)", &[])
        .unwrap();
    db.execute("insert into u values (null, null)", &[])
        .unwrap();
    db.execute("drop index u_ab", &[]).unwrap();
    db.execute("insert into u values (1, 1)", &[]).unwrap();
    assert_eq!(err(&db, "create unique index u_ab on u (a, b)"), 1452);
}

#[test]
fn scripts() {
    let parts = split_script(
        "create table x (a varchar2(5)); -- comment; not a split\ninsert into x values ('a;b');\n/* ; */ select 1 from dual\n/\n",
    );
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert_eq!(parts[1], "insert into x values ('a;b')");
    assert_eq!(
        split_script("select 1 from dual\n/  \t\nselect 2 from dual\n/\r\n").len(),
        2
    );
    let db = db();
    let e = db
        .run_script("create table y (a number); insert into y values ('q')")
        .unwrap_err();
    assert_eq!(e.code, 1722);
    assert!(e.message.contains("insert into y"), "{}", e.message);
}

#[test]
fn hostile_arguments_return_errors_not_panics() {
    let db = db();
    assert_eq!(err(&db, "select date '2024-01-01' + 1e7 from dual"), 1841);
    assert_eq!(
        err(
            &db,
            "select date '2024-01-01' - (-9223372036854775808 / 86400) from dual"
        ),
        1841
    );
    assert_eq!(err(&db, "select add_months(sysdate, 1e9) from dual"), 1841);
    assert_eq!(
        one(&db, "select substr('hello', 2, 1e20) from dual"),
        s("ello")
    );
    assert_eq!(
        one(&db, "select substr('hello', -1e20) from dual"),
        Value::Null
    );
    assert_eq!(
        one(&db, "select instr('hello', 'll', 1e20) from dual"),
        n("0")
    );
    assert_eq!(
        one(&db, "select instr('hello', 'l', -1e20) from dual"),
        n("0")
    );
    assert_eq!(
        one(&db, "select length(lpad('x', 1e12)) from dual"),
        n("32767")
    );
    assert_eq!(one(&db, "select round(1.5, 1e18) from dual"), n("1.5"));
    assert_eq!(one(&db, "select round(12345, -1e18) from dual"), n("0"));
    // Non-ASCII text in a format model: quoted literals pass through, other letters are rejected.
    assert_eq!(
        one(
            &db,
            "select to_char(date '2024-01-01', '\"ıı\"YYYY') from dual"
        ),
        s("ıı2024")
    );
    assert_eq!(
        err(&db, "alter session set nls_date_format = 'ıYYYY'"),
        1821
    );
}

#[test]
fn far_dates_have_distinct_keys() {
    let db = db();
    db.run_script(
        "create table d (x date unique);
         insert into d values (date '9999-12-31');
         insert into d values (date '0001-01-01');",
    )
    .unwrap();
    assert_eq!(one(&db, "select count(distinct x) from d"), n("2"));
}

#[test]
fn statement_failures_and_empty_transactions() {
    let db = emp();
    let mut a = db.session("app");
    // A failing multi-row UPDATE leaves every row as it was.
    a.execute("insert into dept values (40, 'Ops')", &[])
        .unwrap();
    assert_eq!(
        a.execute("update dept set name = 'Same'", &[])
            .unwrap_err()
            .code,
        1
    );
    let r = a
        .execute("select name from dept order by id", &[])
        .unwrap()
        .rows;
    assert_eq!(
        r,
        vec![
            vec![s("Sales")],
            vec![s("Engineering")],
            vec![s("Empty")],
            vec![s("Ops")]
        ]
    );
    // A failing INSERT ... SELECT keeps nothing from the statement.
    assert_eq!(
        a.execute("insert into dept select id + 100, 'X' from dept", &[])
            .unwrap_err()
            .code,
        1
    );
    assert_eq!(
        a.execute("select count(*) from dept", &[]).unwrap().rows[0][0],
        n("4")
    );
    a.rollback();
    // DML that changes nothing does not pin the session to an old snapshot.
    let mut b = db.session("app");
    a.execute("delete from dept where 1 = 0", &[]).unwrap();
    assert!(!a.in_transaction());
    b.execute("create table later (x number)", &[]).unwrap();
    b.execute("insert into dept values (50, 'New')", &[])
        .unwrap();
    b.commit().unwrap();
    assert_eq!(
        a.execute("select count(*) from later", &[]).unwrap().rows[0][0],
        n("0")
    );
    assert_eq!(
        a.execute("select count(*) from dept", &[]).unwrap().rows[0][0],
        n("4")
    );
}
