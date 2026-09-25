// Connects to an already running mock (for example the Docker image) and runs
// one query. Usage: node scripts/smoke.js localhost:1521/FREEPDB1
const oracledb = require('oracledb');

(async () => {
  const connectString = process.argv[2] || 'localhost:1521/FREEPDB1';
  const conn = await oracledb.getConnection({ user: 'test', password: 'oracle', connectString });
  const result = await conn.execute('SELECT 1 FROM DUAL');
  await conn.close();
  if (JSON.stringify(result.rows) !== '[[1]]') {
    throw new Error(`unexpected rows: ${JSON.stringify(result.rows)}`);
  }
  console.log('ok: SELECT 1 FROM DUAL returned [[1]]');
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
