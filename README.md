# mock-oracle

A slim, in-memory stand-in for Oracle Database in CI/CD. It runs Oracle SQL and PL/SQL, and node-oracledb connects to it unchanged.

> Status: early. node-oracledb (Thin mode, versions 6 and 7) connects and runs SQL against in-memory tables (DDL, DML, queries with joins and grouping, transactions, dates), sequences, `RETURNING … INTO`, and the common parts of PL/SQL: anonymous blocks and stored procedures and functions with IN, OUT and IN OUT binds. See [docs/design.md](docs/design.md) for the plan.

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
- **DML:** `INSERT … VALUES`, `INSERT … SELECT`, `UPDATE`, `DELETE`, with row counts, `executeMany`, and statement-level rollback on error. `RETURNING … INTO` returns one value per changed row, including with `executeMany`.
- **Sequences:** `CREATE SEQUENCE` (`START WITH`, `INCREMENT BY`, `MINVALUE`, `MAXVALUE`, `CYCLE`; `CACHE` and `ORDER` are accepted and ignored), `ALTER SEQUENCE` (including `RESTART`), `DROP SEQUENCE`, `NEXTVAL` and per-session `CURRVAL`. As in Oracle, a rollback does not give numbers back. Snapshots include sequence positions.
- **Queries:** `WHERE`, inner/left/right/full/cross joins, comma joins, `GROUP BY`, `HAVING`, aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, …, with `DISTINCT`), `ORDER BY` (with `NULLS FIRST/LAST`, positions and aliases), `OFFSET … FETCH`, `ROWNUM`, `DISTINCT`, `UNION [ALL]`, `INTERSECT`, `MINUS`, scalar, `IN`, `EXISTS` and correlated subqueries, inline views, `CASE`.
- **Functions:** `NVL`, `NVL2`, `COALESCE`, `DECODE`, `NULLIF`, `UPPER`, `LOWER`, `INITCAP`, `LENGTH`, `SUBSTR`, `INSTR`, `REPLACE`, `TRANSLATE`, `TRIM`, `LTRIM`, `RTRIM`, `LPAD`, `RPAD`, `CONCAT`, `CHR`, `ASCII`, `ABS`, `ROUND`, `TRUNC`, `CEIL`, `FLOOR`, `MOD`, `POWER`, `SQRT`, `GREATEST`, `LEAST`, `TO_CHAR`, `TO_NUMBER`, `TO_DATE`, `TO_TIMESTAMP`, `CAST`, `EXTRACT`, `ADD_MONTHS`, `MONTHS_BETWEEN`, `LAST_DAY`, `SYSDATE`, `SYSTIMESTAMP`, `CURRENT_DATE`, `USER`, and date arithmetic.
- **PL/SQL:** anonymous blocks (`BEGIN`, `DECLARE`, `CALL`), `CREATE [OR REPLACE] PROCEDURE` and `FUNCTION`, `DROP PROCEDURE`/`FUNCTION`. Binds come back as OUT and IN OUT values, including `DB_TYPE_BOOLEAN`. Inside blocks: variables and constants (`%TYPE`, `%ROWTYPE`, `BOOLEAN`, `PLS_INTEGER`), `IF`, `CASE`, `LOOP`, `WHILE`, `FOR` over ranges and queries, labels, `EXIT`/`CONTINUE [WHEN]`, `SELECT … INTO`, DML with `SQL%ROWCOUNT`, explicit cursors with parameters and `%FOUND`/`%NOTFOUND`/`%ROWCOUNT`, `EXECUTE IMMEDIATE` (with `INTO` and `USING`), local procedures and functions, recursion, named arguments and defaults, exceptions (predefined, user-defined, `PRAGMA EXCEPTION_INIT`, `RAISE`, `WHEN OTHERS`, `SQLCODE`, `SQLERRM`), `RAISE_APPLICATION_ERROR`, and `DBMS_OUTPUT` (`PUT_LINE`, `GET_LINE`). Stored functions can be called from SQL, where, as in Oracle, they may not change data (ORA-14551). A block that fails undoes its own changes. In scripts and seeds, end each block, procedure or function with a line holding only `/`. Not yet: packages, triggers, collections and records declared with `TYPE`, `BULK COLLECT`, `FORALL` and REF CURSORs.
- **Transactions:** each connection sees its own uncommitted changes; others see them after `COMMIT` (or `autoCommit`). `ROLLBACK`, closing a connection, or a failed commit discard them. DDL commits first, as in Oracle.
- **Sessions:** `ALTER SESSION SET NLS_DATE_FORMAT`, `NLS_TIMESTAMP_FORMAT` and `TIME_ZONE`. JavaScript `Date` values round-trip in the client's local time zone.
- **Errors:** the usual ORA- codes, for example ORA-00001, 00904, 00942, 01400, 01407, 01438, 01476, 01722, 02290, 02289, 06502, 06510, 08002, 12899, 14551, the 20000-20999 range from `RAISE_APPLICATION_ERROR`, and ORA-06550 with PLS- messages for PL/SQL mistakes.

## Develop

```sh
cargo test --workspace
cd crates/mock-oracle-node && npm install && npm run build:debug && npm test
```
