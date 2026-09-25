// End-to-end: sequences, RETURNING INTO and PL/SQL through node-oracledb.

const { test, before, after, beforeEach } = require('node:test');
const assert = require('node:assert/strict');
const oracledb = require('oracledb');
const { MockOracle } = require('..');

const seed = `
  create sequence order_seq start with 1000;
  create table orders (
    id       number primary key,
    customer varchar2(40) not null,
    amount   number(10,2),
    created  date default sysdate
  );
  create table accounts (id number primary key, balance number not null);
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
  end;
  /

  create or replace function balance_of(p_id in number) return number is
    v number;
  begin
    select balance into v from accounts where id = p_id;
    return v;
  end;
  /

  create or replace procedure greet(p_name in varchar2, p_greeting in out varchar2) is
  begin
    p_greeting := p_greeting || ', ' || p_name || '!';
  end;
  /
`;

let db;
let conn;
const connect = () => oracledb.getConnection({ user: 'app', password: 'oracle', connectString: db.connectString });

before(async () => {
  db = await MockOracle.start({ seed });
  conn = await connect();
});

after(async () => {
  await conn?.close();
  await db?.stop();
});

beforeEach(async () => {
  await conn.rollback();
  db.restore();
});

test('sequences hand out NEXTVAL and CURRVAL', async () => {
  const r = await conn.execute('select order_seq.nextval, order_seq.currval from dual');
  assert.deepEqual(r.rows, [[1000, 1000]]);
  await conn.execute("insert into orders (id, customer) values (order_seq.nextval, 'Ada')");
  const ids = await conn.execute('select id from orders');
  assert.deepEqual(ids.rows, [[1001]]);
  await assert.rejects(conn.execute('select nope_seq.nextval from dual'), /ORA-02289/);
});

test('INSERT ... RETURNING INTO returns generated values', async () => {
  const r = await conn.execute(
    `insert into orders (id, customer, amount) values (order_seq.nextval, :customer, :amount)
     returning id, created into :id, :created`,
    {
      customer: 'Ada',
      amount: 12.5,
      id: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
      created: { dir: oracledb.BIND_OUT, type: oracledb.DATE },
    },
  );
  assert.equal(r.rowsAffected, 1);
  assert.deepEqual(r.outBinds.id, [1000]);
  assert.ok(r.outBinds.created[0] instanceof Date);
  assert.ok(Math.abs(r.outBinds.created[0] - Date.now()) < 60_000);

  // Running the same statement again re-executes the cached cursor.
  const again = await conn.execute(
    `insert into orders (id, customer, amount) values (order_seq.nextval, :customer, :amount)
     returning id, created into :id, :created`,
    {
      customer: 'Bob',
      amount: 1,
      id: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
      created: { dir: oracledb.BIND_OUT, type: oracledb.DATE },
    },
  );
  assert.deepEqual(again.outBinds.id, [1001]);
});

test('UPDATE ... RETURNING INTO returns a value per row', async () => {
  await conn.executeMany('insert into orders (id, customer, amount) values (:1, :2, :3)', [
    [1, 'Ada', 10],
    [2, 'Bob', 20],
    [3, 'Cy', 30],
  ]);
  const r = await conn.execute(
    'update orders set amount = amount * 2 where amount > :min returning customer, amount into :c, :a',
    { min: 15, c: { dir: oracledb.BIND_OUT, type: oracledb.STRING, maxSize: 40 }, a: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER } },
  );
  assert.equal(r.rowsAffected, 2);
  assert.deepEqual(r.outBinds, { c: ['Bob', 'Cy'], a: [40, 60] });
  const none = await conn.execute('delete from orders where id = 99 returning id into :id', {
    id: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
  });
  assert.deepEqual(none.outBinds.id, []);
});

test('executeMany with RETURNING INTO returns values per iteration', async () => {
  const r = await conn.executeMany(
    'insert into orders (id, customer) values (order_seq.nextval, :name) returning id into :id',
    [{ name: 'Ada' }, { name: 'Bob' }],
    {
      bindDefs: {
        name: { type: oracledb.STRING, maxSize: 40 },
        id: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
      },
    },
  );
  assert.deepEqual(r.outBinds, [{ id: [1000] }, { id: [1001] }]);
});

test('anonymous blocks return OUT and IN OUT binds', async () => {
  const r = await conn.execute(
    `declare
       v_total number := 0;
     begin
       for r in (select balance from accounts) loop
         v_total := v_total + r.balance;
       end loop;
       :total := v_total;
       :label := upper(:label) || '!';
       :when := date '2024-02-29';
       :flag := v_total > 100;
     end;`,
    {
      total: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
      label: { dir: oracledb.BIND_INOUT, val: 'sum', maxSize: 20 },
      when: { dir: oracledb.BIND_OUT, type: oracledb.DB_TYPE_DATE },
      flag: { dir: oracledb.BIND_OUT, type: oracledb.DB_TYPE_BOOLEAN },
    },
  );
  assert.deepEqual(r.outBinds, { total: 150, label: 'SUM!', when: new Date(2024, 1, 29), flag: true });
});

test('stored procedures take IN, OUT and IN OUT binds', async () => {
  const r = await conn.execute('begin transfer(:from, :to, :amount, :balance); end;', {
    from: 1,
    to: 2,
    amount: 30,
    balance: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
  });
  assert.equal(r.outBinds.balance, 70);
  const again = await conn.execute('begin transfer(:from, :to, :amount, :balance); end;', {
    from: 2,
    to: 1,
    amount: 5,
    balance: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
  });
  assert.equal(again.outBinds.balance, 75);

  const g = await conn.execute('begin greet(:name, :greeting); end;', {
    name: 'Ada',
    greeting: { dir: oracledb.BIND_INOUT, val: 'Hello', maxSize: 50 },
  });
  assert.equal(g.outBinds.greeting, 'Hello, Ada!');

  const f = await conn.execute('begin :b := balance_of(:id); end;', {
    b: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
    id: 1,
  });
  assert.equal(f.outBinds.b, 75);
  const q = await conn.execute('select id, balance_of(id) from accounts order by id');
  assert.deepEqual(q.rows, [[1, 75], [2, 75]]);
});

test('errors raised in PL/SQL reach the client and undo the block', async () => {
  await assert.rejects(
    conn.execute('begin transfer(2, 1, 1000, :b); end;', { b: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER } }),
    (e) => e.errorNum === 20001 && /ORA-20001: Insufficient funds/.test(e.message),
  );
  await assert.rejects(
    conn.execute(`begin
        update accounts set balance = 0;
        raise_application_error(-20002, 'stop');
      end;`),
    /ORA-20002: stop/,
  );
  const r = await conn.execute('select sum(balance) from accounts');
  assert.deepEqual(r.rows, [[150]]);
  // An OUT value that does not fit fails the block and undoes its changes.
  await assert.rejects(
    conn.execute("begin update accounts set balance = 0; :s := 'too long for the buffer'; end;", {
      s: { dir: oracledb.BIND_OUT, type: oracledb.STRING, maxSize: 5 },
    }),
    /ORA-06502/,
  );
  const after = await conn.execute('select sum(balance) from accounts');
  assert.deepEqual(after.rows, [[150]]);
});

test('the drop-if-exists pattern and DBMS_OUTPUT work', async () => {
  const dropIfExists = `begin
      execute immediate 'drop table scratch';
    exception
      when others then
        if sqlcode != -942 then raise; end if;
    end;`;
  await conn.execute(dropIfExists);
  await conn.execute('create table scratch (id number)');
  await conn.execute(dropIfExists);

  await conn.execute(`begin
      dbms_output.enable(null);
      dbms_output.put_line('hello');
      dbms_output.put_line('world');
    end;`);
  const lines = [];
  for (;;) {
    const r = await conn.execute('begin dbms_output.get_line(:line, :status); end;', {
      line: { dir: oracledb.BIND_OUT, type: oracledb.STRING, maxSize: 32767 },
      status: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER },
    });
    if (r.outBinds.status !== 0) break;
    lines.push(r.outBinds.line);
  }
  assert.deepEqual(lines, ['hello', 'world']);
});

test('procedures created over the connection can be called', async () => {
  await conn.execute(`create or replace function add_tax(p number) return number is
    begin
      return round(p * 1.2, 2);
    end;`);
  const r = await conn.execute('select add_tax(10) from dual');
  assert.deepEqual(r.rows, [[12]]);
  const call = await conn.execute('call transfer(1, 2, 10, :b)', { b: { dir: oracledb.BIND_OUT, type: oracledb.NUMBER } });
  assert.equal(call.outBinds.b, 90);
});
