# mock-oracle

A slim, in-memory stand-in for Oracle Database in CI/CD. It runs Oracle SQL and PL/SQL, and node-oracledb connects to it unchanged.

> Status: early. node-oracledb (Thin mode, versions 6 and 7) connects and runs SQL against in-memory tables: DDL, DML, queries with joins and grouping, transactions, dates. PL/SQL and sequences come next; see [docs/design.md](docs/design.md) for the plan.

## Layout

| Crate | What it is |
|---|---|
| `crates/mock-oracle-core` | The engine: SQL/PL/SQL parser, executor, in-memory storage. No I/O. |
| `crates/mock-oracle-server` | TNS listener for thin drivers; also the Docker image. |
| `crates/mock-oracle-node` | npm package `mock-oracle` (napi-rs). Starts the server inside your Node process. |

## Use from Node

```js
const { MockOracle } = require('mock-oracle');
const oracledb = require('oracledb');

const db = await MockOracle.start({
  seed: `
    create table dept (id number(4) primary key, name varchar2(30) not null);
    insert into dept values (10, 'Sales');
  `,
});                                            // picks a free port; pass { port, password } to choose
const conn = await oracledb.getConnection({ user: 'test', password: 'oracle', connectString: db.connectString });
const { rows } = await conn.execute('select name from dept');   // [['Sales']]

db.restore();                                  // back to the seeded state, e.g. in beforeEach
const snap = db.snapshot();                    // or save and restore any state
db.restore(snap);
db.runScript("insert into dept values (20, 'Ops')");   // run SQL without a connection

await conn.close();
await db.stop();
```

## Use from Docker

```sh
docker build -t mock-oracle .
docker run -p 1521:1521 -v "$PWD/sql:/docker-entrypoint-initdb.d" mock-oracle
# connect with any user name, password "oracle", connect string localhost:1521/FREEPDB1
```

At startup the server runs every `.sql` file in `/docker-entrypoint-initdb.d`, in name order. Point `MOCK_ORACLE_SEED` at another file or directory to change that.

## Logging in

Any user name and any service name are accepted. Every user shares one password: `oracle` by default. Change it with `MockOracle.start({ password })` or the `MOCK_ORACLE_PASSWORD` environment variable. Wrong passwords fail with ORA-01017, as on a real database.

## What works

- **Tables:** `CREATE TABLE` (including `AS SELECT`), `DROP TABLE`, `TRUNCATE TABLE`, `CREATE [UNIQUE] INDEX`, `DROP INDEX`. Types `NUMBER(p,s)`, `INTEGER`, `VARCHAR2`, `CHAR`, `DATE`, `TIMESTAMP(p)`; `CLOB` is stored as a long string. `NOT NULL`, `PRIMARY KEY`, `UNIQUE`, `CHECK`, `DEFAULT` and identity columns are enforced; foreign keys are accepted but not enforced yet.
- **DML:** `INSERT … VALUES`, `INSERT … SELECT`, `UPDATE`, `DELETE`, with row counts, `executeMany`, and statement-level rollback on error.
- **Queries:** `WHERE`, inner/left/right/full/cross joins, comma joins, `GROUP BY`, `HAVING`, aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, …, with `DISTINCT`), `ORDER BY` (with `NULLS FIRST/LAST`, positions and aliases), `OFFSET … FETCH`, `ROWNUM`, `DISTINCT`, `UNION [ALL]`, `INTERSECT`, `MINUS`, scalar, `IN`, `EXISTS` and correlated subqueries, inline views, `CASE`.
- **Functions:** `NVL`, `NVL2`, `COALESCE`, `DECODE`, `NULLIF`, `UPPER`, `LOWER`, `INITCAP`, `LENGTH`, `SUBSTR`, `INSTR`, `REPLACE`, `TRANSLATE`, `TRIM`, `LTRIM`, `RTRIM`, `LPAD`, `RPAD`, `CONCAT`, `CHR`, `ASCII`, `ABS`, `ROUND`, `TRUNC`, `CEIL`, `FLOOR`, `MOD`, `POWER`, `SQRT`, `GREATEST`, `LEAST`, `TO_CHAR`, `TO_NUMBER`, `TO_DATE`, `TO_TIMESTAMP`, `CAST`, `EXTRACT`, `ADD_MONTHS`, `MONTHS_BETWEEN`, `LAST_DAY`, `SYSDATE`, `SYSTIMESTAMP`, `CURRENT_DATE`, `USER`, and date arithmetic.
- **Transactions:** each connection sees its own uncommitted changes; others see them after `COMMIT` (or `autoCommit`). `ROLLBACK`, closing a connection, or a failed commit discard them. DDL commits first, as in Oracle.
- **Sessions:** `ALTER SESSION SET NLS_DATE_FORMAT`, `NLS_TIMESTAMP_FORMAT` and `TIME_ZONE`. JavaScript `Date` values round-trip in the client's local time zone.
- **Errors:** the usual ORA- codes, for example ORA-00001, 00904, 00942, 01400, 01407, 01438, 01476, 01722, 02290 and 12899.

## Develop

```sh
cargo test --workspace
cd crates/mock-oracle-node && npm install && npm run build:debug && npm test
```
