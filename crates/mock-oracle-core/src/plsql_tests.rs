//! Tests of sequences, RETURNING and PL/SQL through the public API.

use std::sync::Arc;

use crate::*;

fn db() -> Arc<Database> {
    Arc::new(Database::new())
}

fn n(x: i64) -> Value {
    Value::number(x)
}

fn s(x: &str) -> Value {
    Value::varchar(x)
}

fn one(db: &Arc<Database>, sql: &str) -> Value {
    let r = db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(r.rows.len(), 1, "{sql}");
    r.rows[0][0].clone()
}

fn err(session: &mut Session, sql: &str, binds: &[Value]) -> OraError {
    session
        .execute(sql, binds)
        .map(|_| panic!("{sql} should fail"))
        .unwrap_err()
}

/// Runs a block and returns its bind values afterwards.
fn block(session: &mut Session, sql: &str, binds: &[Value]) -> Vec<Value> {
    session
        .execute(sql, binds)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .out_binds
        .into_iter()
        .map(|b| match b {
            OutBind::Value(v) => v,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn sequences() {
    let db = db();
    db.run_script(
        "create sequence s1;
         create sequence s2 start with 100 increment by -10 minvalue 70 maxvalue 100 cycle nocache;",
    )
    .unwrap();
    let mut a = db.session("A");
    let e = err(&mut a, "select s1.currval from dual", &[]);
    assert_eq!(e.code, 8002);
    let next = |ses: &mut Session, sql: &str| ses.execute(sql, &[]).unwrap().rows[0][0].clone();
    assert_eq!(next(&mut a, "select s1.nextval from dual"), n(1));
    assert_eq!(next(&mut a, "select s1.nextval from dual"), n(2));
    assert_eq!(next(&mut a, "select s1.currval from dual"), n(2));
    // Other sessions share the counter but not CURRVAL.
    let mut b = db.session("B");
    assert_eq!(next(&mut b, "select s1.nextval from dual"), n(3));
    assert_eq!(next(&mut a, "select s1.currval from dual"), n(2));
    // Rollback does not give numbers back.
    a.execute("create table t (id number, v varchar2(10))", &[])
        .unwrap();
    a.execute("insert into t values (s1.nextval, 'x')", &[])
        .unwrap();
    a.rollback();
    assert_eq!(next(&mut a, "select s1.nextval from dual"), n(5));
    let cycle: Vec<Value> = (0..5)
        .map(|_| next(&mut a, "select s2.nextval from dual"))
        .collect();
    assert_eq!(cycle, [n(100), n(90), n(80), n(70), n(100)]);

    db.execute("create sequence s3 maxvalue 2", &[]).unwrap();
    let mut c = db.session("C");
    next(&mut c, "select s3.nextval from dual");
    next(&mut c, "select s3.nextval from dual");
    assert_eq!(err(&mut c, "select s3.nextval from dual", &[]).code, 8004);

    db.execute("alter sequence s1 increment by 100", &[])
        .unwrap();
    assert_eq!(next(&mut a, "select s1.nextval from dual"), n(105));
    db.execute("alter sequence s1 restart start with 7", &[])
        .unwrap();
    assert_eq!(next(&mut a, "select s1.nextval from dual"), n(7));

    assert_eq!(err(&mut a, "create sequence s1", &[]).code, 955);
    db.execute("drop sequence s1", &[]).unwrap();
    assert_eq!(err(&mut a, "select s1.nextval from dual", &[]).code, 2289);
    assert_eq!(err(&mut a, "drop sequence s1", &[]).code, 2289);
}

#[test]
fn snapshots_restore_sequences() {
    let db = db();
    db.execute("create sequence s", &[]).unwrap();
    let snap = db.snapshot();
    one(&db, "select s.nextval from dual");
    one(&db, "select s.nextval from dual");
    db.restore(&snap);
    assert_eq!(one(&db, "select s.nextval from dual"), n(1));
}

#[test]
fn returning_into_binds() {
    let db = db();
    db.run_script(
        "create table t (id number generated always as identity, name varchar2(20));
         insert into t (name) values ('a');
         insert into t (name) values ('b');",
    )
    .unwrap();
    assert_eq!(
        bind_directions("insert into t (name) values (:n) returning id, name into :id, :nm")
            .unwrap(),
        [BindDir::In, BindDir::Returning, BindDir::Returning]
    );
    let mut ses = db.session("U");
    let r = ses
        .execute(
            "insert into t (name) values (:n) returning id, upper(name) into :id, :nm",
            &[s("c"), Value::Null, Value::Null],
        )
        .unwrap();
    assert_eq!(
        r.out_binds,
        [
            OutBind::In,
            OutBind::Returning(vec![n(3)]),
            OutBind::Returning(vec![s("C")])
        ]
    );
    let r = ses
        .execute(
            "update t set name = name || '!' where id < 3 returning name into :x",
            &[Value::Null],
        )
        .unwrap();
    assert_eq!(r.rows_affected, 2);
    assert_eq!(r.out_binds, [OutBind::Returning(vec![s("a!"), s("b!")])]);
    let r = ses
        .execute(
            "delete from t where id = 99 return id into :x",
            &[Value::Null],
        )
        .unwrap();
    assert_eq!(r.out_binds, [OutBind::Returning(vec![])]);
}

#[test]
fn anonymous_blocks() {
    let db = db();
    db.run_script(
        "create table emp (id number primary key, name varchar2(20), salary number(8,2));
         insert into emp values (1, 'Ada', 100);
         insert into emp values (2, 'Bob', 200);
         insert into emp values (3, 'Cy', 300);",
    )
    .unwrap();
    let mut ses = db.session("U");
    // Binds are numbered by name in PL/SQL.
    let out = block(
        &mut ses,
        "declare
            total number := 0;
            i pls_integer;
         begin
            for r in (select salary from emp order by id) loop
                total := total + r.salary;
            end loop;
            :total := total;
            select name into :name from emp where id = :id;
            i := 0;
            while i < 5 loop
                i := i + 1;
                continue when mod(i, 2) = 0;
                :odd := nvl(:odd, 0) + i;
            end loop;
            :flag := case when total > 500 then 'big' else 'small' end;
            :id := :id + 1;
         end;",
        &[Value::Null, Value::Null, n(2), Value::Null, Value::Null],
    );
    assert_eq!(out, [n(600), s("Bob"), n(3), n(9), s("big")]);

    // An error inside a block undoes the block's changes.
    let e = err(
        &mut ses,
        "begin
            update emp set salary = salary * 2;
            insert into emp values (1, 'Dup', 1);
         end;",
        &[],
    );
    assert_eq!(e.code, 1);
    assert_eq!(
        ses.execute("select sum(salary) from emp", &[])
            .unwrap()
            .rows[0][0],
        n(600)
    );
    // Handlers.
    let out = block(
        &mut ses,
        "declare
            v emp.name%type;
            e_custom exception;
         begin
            begin
                select name into v from emp where id = 42;
            exception
                when no_data_found then :a := 'none';
            end;
            begin
                select name into v from emp;
            exception
                when too_many_rows then :b := sqlcode;
            end;
            begin
                raise e_custom;
            exception
                when e_custom then :c := sqlerrm;
            end;
            begin
                :d := 1 / 0;
            exception
                when others then :d := sqlcode;
            end;
         end;",
        &[Value::Null, Value::Null, Value::Null, Value::Null],
    );
    assert_eq!(
        out,
        [s("none"), n(-1422), s("User-Defined Exception"), n(-1476)]
    );

    let e = err(
        &mut ses,
        "begin raise_application_error(-20001, 'Bad input: ' || :x); end;",
        &[s("42")],
    );
    assert_eq!((e.code, e.message.as_str()), (20001, "Bad input: 42"));
    let e = err(
        &mut ses,
        "declare x varchar2(2); begin x := 'abc'; end;",
        &[],
    );
    assert_eq!(e.code, 6502);
    let e = err(&mut ses, "declare e exception; begin raise e; end;", &[]);
    assert_eq!(e.code, 6510);
    let e = err(&mut ses, "begin :x := nosuch; end;", &[Value::Null]);
    assert_eq!(e.code, 6550);
}

#[test]
fn cursors_and_records() {
    let db = db();
    db.run_script(
        "create table emp (id number primary key, name varchar2(20));
         insert into emp values (1, 'Ada');
         insert into emp values (2, 'Bob');",
    )
    .unwrap();
    let mut ses = db.session("U");
    let out = block(
        &mut ses,
        "declare
            cursor c(p_min number) is select id, name from emp where id >= p_min order by id;
            r c%rowtype;
            e emp%rowtype;
            names varchar2(100);
         begin
            open c(1);
            loop
                fetch c into r;
                exit when c%notfound;
                names := names || r.name || ',';
            end loop;
            :count := c%rowcount;
            close c;
            select * into e from emp where id = 2;
            :e := e.id || '=' || e.name;
            update emp set name = upper(name);
            :updated := sql%rowcount;
            :names := names;
         end;",
        &[Value::Null, Value::Null, Value::Null, Value::Null],
    );
    assert_eq!(out, [n(2), s("2=Bob"), n(2), s("Ada,Bob,")]);
}

#[test]
fn procedures_and_functions() {
    let db = db();
    db.run_script(
        "create table accounts (id number primary key, balance number not null);
         insert into accounts values (1, 100);
         insert into accounts values (2, 50);

         create or replace procedure transfer(
             p_from in number, p_to in number, p_amount in number, p_new_balance out number
         ) as
             v_balance accounts.balance%type;
         begin
             select balance into v_balance from accounts where id = p_from;
             if v_balance < p_amount then
                 raise_application_error(-20001, 'Insufficient funds');
             end if;
             update accounts set balance = balance - p_amount where id = p_from;
             update accounts set balance = balance + p_amount where id = p_to;
             select balance into p_new_balance from accounts where id = p_from;
         end transfer;
         /

         create function balance_of(p_id number) return number is
             v number;
         begin
             select balance into v from accounts where id = p_id;
             return v;
         exception
             when no_data_found then return null;
         end;
         /

         create function fact(n pls_integer) return number deterministic is
         begin
             if n <= 1 then return 1; end if;
             return n * fact(n - 1);
         end;
         /",
    )
    .unwrap();
    let mut ses = db.session("U");
    let out = block(
        &mut ses,
        "begin transfer(1, 2, 30, :bal); end;",
        &[Value::Null],
    );
    assert_eq!(out, [n(70)]);
    let out = block(
        &mut ses,
        "begin transfer(p_to => 1, p_from => 2, p_amount => :amt, p_new_balance => :bal); end;",
        &[n(10), Value::Null],
    );
    assert_eq!(out, [n(10), n(70)]);
    let e = err(
        &mut ses,
        "begin transfer(1, 2, 1000, :b); end;",
        &[Value::Null],
    );
    assert_eq!((e.code, e.message.as_str()), (20001, "Insufficient funds"));
    let e = err(&mut ses, "begin transfer(1, 2); end;", &[]);
    assert_eq!(e.code, 6550);
    // Functions work from SQL and PL/SQL.
    let r = ses
        .execute(
            "select id, balance_of(id), fact(5) from accounts order by id",
            &[],
        )
        .unwrap();
    assert_eq!(r.rows, [[n(1), n(80), n(120)], [n(2), n(70), n(120)]]);
    let out = block(
        &mut ses,
        "begin :x := balance_of(99); :y := fact(3); end;",
        &[Value::Null, Value::Null],
    );
    assert_eq!(out, [Value::Null, n(6)]);
    // CALL.
    let out = block(&mut ses, "call transfer(2, 1, 5, :b)", &[Value::Null]);
    assert_eq!(out, [n(65)]);
    // A function called from SQL may not change data.
    db.run_script(
        "create function bump return number is begin update accounts set balance = 0; return 1; end;\n/",
    )
    .unwrap();
    assert_eq!(err(&mut ses, "select bump from dual", &[]).code, 14551);
    assert_eq!(
        err(
            &mut ses,
            "create procedure transfer is begin null; end;",
            &[]
        )
        .code,
        955
    );
    ses.execute("drop function bump", &[]).unwrap();
    assert_eq!(err(&mut ses, "drop function bump", &[]).code, 4043);
    assert_eq!(err(&mut ses, "drop procedure fact", &[]).code, 4043);
}

#[test]
fn dynamic_sql_and_output() {
    let db = db();
    let mut ses = db.session("U");
    let drop_if_exists = "begin
            execute immediate 'drop table t';
         exception
            when others then
                if sqlcode != -942 then raise; end if;
         end;";
    block(&mut ses, drop_if_exists, &[]);
    ses.execute("create table t (id number)", &[]).unwrap();
    block(&mut ses, drop_if_exists, &[]);
    block(
        &mut ses,
        "declare v number; begin
            execute immediate 'create table t (id number, name varchar2(10))';
            execute immediate 'insert into t values (:1, :2)' using 1, 'one';
            execute immediate 'select count(*) from t where id = :1' into v using 1;
            dbms_output.enable;
            dbms_output.put_line('count=' || v);
            dbms_output.put('a');
            dbms_output.put_line('b');
         end;",
        &[],
    );
    let out = block(
        &mut ses,
        "begin dbms_output.get_line(:line, :status); end;",
        &[Value::Null, Value::Null],
    );
    assert_eq!(out, [s("count=1"), n(0)]);
    let out = block(
        &mut ses,
        "begin dbms_output.get_line(:line, :status); end;",
        &[Value::Null, Value::Null],
    );
    assert_eq!(out, [s("ab"), n(0)]);
    let out = block(
        &mut ses,
        "begin dbms_output.get_line(:line, :status); end;",
        &[Value::Null, Value::Null],
    );
    assert_eq!(out, [Value::Null, n(1)]);
}

#[test]
fn scripts_keep_plsql_units_whole() {
    assert_eq!(
        split_script(
            "create table t (x number);
             begin
               insert into t values (1);
               insert into t values (2);
             end;
             /
             create or replace procedure p as begin null; end;
             /
             select 1 from dual;"
        )
        .len(),
        4
    );
}

#[test]
fn booleans_and_nested_routines() {
    let db = db();
    let mut ses = db.session("U");
    let out = block(
        &mut ses,
        "declare
            ok boolean := true;
            function twice(x number) return number is begin return x * 2; end;
            procedure bump(x in out number) is begin x := x + 1; end;
            v number := 1;
         begin
            if ok and not (1 > 2) then
                bump(v);
                :r := twice(v);
            end if;
            <<outer>>
            for i in 1 .. 3 loop
                for j in reverse 1 .. 3 loop
                    exit outer when i * j = 6;
                    :last := i * 10 + j;
                end loop;
            end loop outer;
         end;",
        &[Value::Null, Value::Null],
    );
    assert_eq!(out, [n(4), n(11)]);
}

#[test]
fn update_returning_bind_positions() {
    assert_eq!(
        bind_directions(
            "update orders set amount = amount * 2 where amount > :min returning customer, amount into :c, :a"
        )
        .unwrap(),
        [BindDir::In, BindDir::Returning, BindDir::Returning]
    );
}

#[test]
fn nextval_advances_once_per_row() {
    let db = db();
    db.run_script(
        "create sequence s;
         create table t (id number, ref number, note varchar2(10) default 'x');
         create table src (x number);
         insert into src values (1);
         insert into src values (2);
         insert into src values (3);",
    )
    .unwrap();
    let mut ses = db.session("U");
    let row = |ses: &mut Session, sql: &str| ses.execute(sql, &[]).unwrap().rows.remove(0);
    // CURRVAL before NEXTVAL in the same row sees the new value, even in a new session.
    assert_eq!(
        row(&mut ses, "select s.currval, s.nextval, s.nextval from dual"),
        [n(1), n(1), n(1)]
    );
    ses.execute("insert into t (id, ref) values (s.nextval, s.nextval)", &[])
        .unwrap();
    assert_eq!(row(&mut ses, "select id, ref from t"), [n(2), n(2)]);
    // One value per row.
    let r = ses
        .execute("select s.nextval, x from src order by x", &[])
        .unwrap();
    let ids: Vec<Value> = r.rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(ids, [n(3), n(4), n(5)]);
    ses.execute("update t set id = s.nextval, ref = s.currval", &[])
        .unwrap();
    assert_eq!(row(&mut ses, "select id, ref from t"), [n(6), n(6)]);
    ses.execute("insert into t (id, ref) select s.nextval, x from src", &[])
        .unwrap();
    assert_eq!(
        ses.execute("select count(distinct id) from t where ref < 5", &[])
            .unwrap()
            .rows[0][0],
        n(3)
    );
    // Outside a row, each NEXTVAL in PL/SQL advances.
    let out = block(
        &mut ses,
        "begin :a := s.nextval; :b := s.nextval; end;",
        &[Value::Null, Value::Null],
    );
    assert_eq!(out, [n(10), n(11)]);
}

#[test]
fn skipped_operands_do_not_call_functions() {
    let db = db();
    db.run_script(
        "create function boom return number is
         begin
             raise_application_error(-20001, 'should not run');
         end;
         /
         create function one return number is begin return 1; end;
         /",
    )
    .unwrap();
    let mut ses = db.session("U");
    let out = block(
        &mut ses,
        "declare n number := 0;
         begin
             :a := case when n = 0 then 0 else boom() end;
             :b := nvl(1, boom());
             :c := coalesce(null, one(), boom());
             :d := decode(one(), 1, 'one', boom());
             :e := nvl2(null, boom(), 'else');
             if n = 1 and boom() = 1 then :f := 'bad'; else :f := 'and'; end if;
             if one() = 1 or boom() = 1 then :g := 'or'; end if;
             :h := case one() when 2 then boom() when 1 then 'hit' end;
         end;",
        &vec![Value::Null; 8],
    );
    assert_eq!(
        out,
        [
            n(0),
            n(1),
            n(1),
            s("one"),
            s("else"),
            s("and"),
            s("or"),
            s("hit")
        ]
    );
    // The branch that is taken still runs.
    let e = err(
        &mut ses,
        "begin :x := case when 1 = 1 then boom() end; end;",
        &[Value::Null],
    );
    assert_eq!(e.code, 20001);
}

#[test]
fn dbms_output_limit_counts_only_buffered_bytes() {
    let db = db();
    let mut ses = db.session("U");
    // 1.5 MB written in total, but never more than 1 KB held at once.
    block(
        &mut ses,
        "declare line varchar2(2000); status number;
         begin
             dbms_output.enable;
             for i in 1 .. 1500 loop
                 dbms_output.put_line(rpad('x', 1000, 'x'));
                 dbms_output.get_line(line, status);
             end loop;
         end;",
        &[],
    );
}
