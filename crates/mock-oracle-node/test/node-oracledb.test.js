// End-to-end: start the mock from the npm package and talk to it with the
// real node-oracledb driver (Thin mode), exactly as an application would.

const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const oracledb = require('oracledb');
const { MockOracle } = require('..');

let db;
let conn;

before(async () => {
  db = await MockOracle.start();
  conn = await oracledb.getConnection({ user: 'test', password: 'oracle', connectString: db.connectString });
});

after(async () => {
  await conn?.close();
  await db?.stop();
});

test('SELECT 1 FROM DUAL', async () => {
  const result = await conn.execute('SELECT 1 FROM DUAL');
  assert.deepEqual(result.rows, [[1]]);
  assert.equal(result.metaData[0].name, '1');
  assert.equal(result.metaData[0].dbType, oracledb.DB_TYPE_NUMBER);
});

test('expressions, strings and NULLs', async () => {
  const result = await conn.execute(
    "select 2 * (3 + 4) n, 'a' || 1 s, '' e, dummy from dual",
    [],
    { outFormat: oracledb.OUT_FORMAT_OBJECT },
  );
  assert.deepEqual(result.rows, [{ N: 14, S: 'a1', E: null, DUMMY: 'X' }]);
});

test('bind variables, including re-executing the same statement', async () => {
  const result = await conn.execute('select :a + 1, :b from dual', { a: 41, b: 'héllo' });
  assert.deepEqual(result.rows, [[42, 'héllo']]);
  for (const n of [1, 2, 3]) {
    const again = await conn.execute('select :n * 2 from dual', [n]);
    assert.deepEqual(again.rows, [[n * 2]]);
  }
});

test('result sets and fetching without prefetch', async () => {
  const rs = (await conn.execute('select 42 from dual', [], { resultSet: true })).resultSet;
  assert.deepEqual(await rs.getRows(10), [[42]]);
  await rs.close();
  const noPrefetch = await conn.execute("select 'x' from dual", [], { prefetchRows: 0 });
  assert.deepEqual(noPrefetch.rows, [['x']]);
});

test('Oracle errors come back as ORA- codes and the session keeps working', async () => {
  await assert.rejects(conn.execute('select 1/0 from dual'), { errorNum: 1476 });
  await assert.rejects(conn.execute('select 1 from missing_table'), { errorNum: 942 });
  const ok = await conn.execute("select 'still ok' from dual");
  assert.deepEqual(ok.rows, [['still ok']]);
});

test('commit and ping succeed', async () => {
  await conn.commit();
  await conn.ping();
});

test('a wrong password is refused with ORA-01017', async () => {
  await assert.rejects(
    oracledb.getConnection({ user: 'test', password: 'wrong', connectString: db.connectString }),
    { errorNum: 1017 },
  );
});

test('the password can be configured', async () => {
  const custom = await MockOracle.start({ password: 's3cret' });
  try {
    const c = await oracledb.getConnection({ user: 'app', password: 's3cret', connectString: custom.connectString });
    assert.deepEqual((await c.execute('select 1 from dual')).rows, [[1]]);
    await c.close();
  } finally {
    await custom.stop();
  }
});
