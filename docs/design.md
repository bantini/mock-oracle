# mock-oracle: design plan

Status: draft, 2026-09-25. Language: Rust (confirmed by nilayan).

## Goal

mock-oracle is a slim, fast, in-memory stand-in for Oracle Database in CI/CD. It runs Oracle-flavoured SQL and PL/SQL, and it can be used in two ways:

- as a library inside the test process (Node first, then Python, Java and others)
- as a Docker image that real Oracle drivers connect to over the network

It is not a full Oracle. The target is the subset application tests actually use, with Oracle's semantics and ORA- error codes where tests depend on them.

## Architecture

```
                 ┌────────────────────────────────────────┐
                 │           mock-oracle-core (Rust)       │
                 │ lexer → parser → planner → executor     │
                 │ PL/SQL interpreter · in-memory storage   │
                 │ transactions · catalog · ORA- errors     │
                 └───────────────┬────────────────────────┘
        ┌──────────────┬─────────┼──────────┬───────────────┐
  mock-oracle-node  -python   -java      -ffi (C ABI)   mock-oracle-server
   (napi-rs, npm)  (PyO3,pip) (JNI/FFM)  (Go, .NET, …)  (TNS listener, Docker)
```

It is a single Cargo workspace:

| Crate | Purpose |
|---|---|
| `mock-oracle-core` | The whole engine. It has no I/O and no language-specific code. |
| `mock-oracle-node` | The npm package, built with napi-rs. Prebuilt binaries for linux-x64/arm64, macOS and Windows, so users never compile. |
| `mock-oracle-server` | A standalone binary that speaks Oracle's TNS/TTC wire protocol on port 1521. It becomes the Docker image. |
| `mock-oracle-ffi` | A C ABI, for any other language. |
| `mock-oracle-python`, `mock-oracle-java` | Later bindings, each a thin layer over core. |

Rule: every behaviour lives in core. A binding only converts values and errors.

## Core engine

**Parser.** A hand-written recursive-descent parser for Oracle SQL and PL/SQL. Existing Rust SQL parsers (sqlparser-rs) have only thin Oracle support and no PL/SQL. A parser we own lets us match Oracle's quirks and emit accurate ORA- syntax errors.

**Storage.** In-memory tables. Transactions use copy-on-write snapshots, so COMMIT and ROLLBACK are cheap. There are also two test helpers:

- `snapshot()` / `restore()`, which reset the database between tests in microseconds
- a schema seed that is loaded once and cloned per test

**Oracle semantics that matter from day one:**

- The empty string `''` is NULL.
- Types: `NUMBER(p,s)` as exact decimal, `VARCHAR2`, `CHAR`, `DATE` (which has a time part), `TIMESTAMP`, `CLOB`, `BLOB`, `RAW`.
- Uppercased unquoted identifiers, `DUAL`, `ROWNUM`, `ROWID`.
- Bind variables `:name` and `:1`.

## Feature scope by phase

Hard requirement (nilayan, 2026-09-25): once the mock is running, node-oracledb must be able to connect to it. The wire-protocol server is therefore part of phase 1, not a later add-on.

**Phase 1: wire protocol and SQL core**

Step 1 is a thin end-to-end slice, which tackles the biggest risk (the protocol) first:
- node-oracledb (Thin mode) connects to `mock-oracle-server`, authenticates, runs `SELECT 1 FROM DUAL` and gets the row back.
- The protocol is not publicly documented. The open-source python-oracledb thin driver is the working reference.
- The same server can be started from the npm package (`db.listen()`) or from Docker.

The protocol then grows alongside the SQL features:
- **Protocol:** TNS connect and O5LOGON auth, execute, fetch (row prefetch and array fetch), bind variables, commit, rollback, `RETURNING INTO`, LOB reads, and ORA- errors reported the way the driver expects.
- **DDL:** `CREATE`/`DROP TABLE`, constraints (PK, FK, UNIQUE, NOT NULL, CHECK), `CREATE SEQUENCE`, identity columns, indexes (accepted; used for uniqueness only), views.
- **DML:** `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `RETURNING … INTO`.
- **Queries:**
  - joins, including Oracle's `(+)`
  - subqueries, `GROUP BY`/`HAVING`, `UNION`/`MINUS`/`INTERSECT`
  - `ORDER BY … NULLS FIRST/LAST`, `FETCH FIRST`/`OFFSET`, and `ROWNUM` paging
  - `CONNECT BY`
- **Functions:** `NVL`, `NVL2`, `DECODE`, `COALESCE`, `CASE`, `TO_CHAR`, `TO_DATE`, `TO_NUMBER`, `SYSDATE`, `SYSTIMESTAMP`, `TRUNC`, `ROUND`, `SUBSTR`, `INSTR`, `LENGTH`, `UPPER`, `LOWER`, `TRIM`, `LPAD`, `RPAD`, `REPLACE`, `ADD_MONTHS`, and the aggregates.
- **Sequences:** `seq.NEXTVAL` and `CURRVAL`.
- **Transactions:** `COMMIT`, `ROLLBACK`, `SAVEPOINT`.
- **Docker:** a static binary on a distroless image of a few MB. Usage: `docker run -p 1521:1521 mock-oracle`. It can take seed SQL from a mounted `/docker-entrypoint-initdb.d`.

**Phase 2: PL/SQL (over the wire as well)**

- Anonymous blocks: `DECLARE … BEGIN … EXCEPTION … END`.
- Variables, `%TYPE`, `%ROWTYPE`, constants, records.
- Control flow: `IF`, `CASE`, `LOOP`, `WHILE`, numeric and cursor `FOR` loops, `EXIT WHEN`.
- `SELECT … INTO`, explicit cursors, `SQL%ROWCOUNT`/`%FOUND`/`%NOTFOUND`.
- Exceptions: `NO_DATA_FOUND`, `TOO_MANY_ROWS`, `DUP_VAL_ON_INDEX`, user exceptions, `RAISE_APPLICATION_ERROR`.
- Stored `PROCEDURE`, `FUNCTION` and `PACKAGE` (spec and body), with IN/OUT/IN OUT parameters. Callable from node-oracledb with `BEGIN proc(:a, :b); END;` and OUT binds.
- Triggers (row-level BEFORE/AFTER).
- `EXECUTE IMMEDIATE`, `DBMS_OUTPUT` (readable by the driver through `DBMS_OUTPUT.GET_LINE`).
- REF CURSOR out-params.

**Phase 3: other drivers and bindings**

- Verify that python-oracledb thin and JDBC thin connect to the same server. They share the protocol, so the work should be small.
- In-process bindings: Python (PyO3), Java (JNI or the FFM API), and a C ABI for everything else.

## Node usage (phase 1)

The main path is plain node-oracledb pointed at the mock. The npm package can start the server inside the test process, so Docker is not required:

```js
const { MockOracle } = require('mock-oracle');
const oracledb = require('oracledb');

const db = await MockOracle.start({ port: 0, seed: ['schema.sql'] }); // port 0 = pick a free one
const conn = await oracledb.getConnection({
  user: 'test', password: 'test', connectString: db.connectString }); // e.g. localhost:41234/FREEPDB1
const r = await conn.execute('SELECT id, name FROM users WHERE id = :id', { id: 1 });

db.snapshot(); db.restore();   // reset data between tests
await db.stop();
```

In CI you can run it as a Docker service container on 1521 instead, and the application code stays the same.

## Testing strategy

- **Conformance corpus.** Each case is a `.sql` file paired with its expected output. The expected results are recorded once against a real Oracle Free container (`gvenzl/oracle-free`), so the mock is checked against real Oracle behaviour, not our own assumptions.
- **Rust unit tests** in core, and **binding smoke tests** for each language.
- **Driver tests** from phase 1 (node-oracledb), extended in phase 3: the real node-oracledb, python-oracledb and JDBC drivers run against the server.
- **CI:** GitHub Actions builds the prebuilt npm binaries and the Docker image.

## Open questions for nilayan

1. **Repository:** which GitHub repo should this live in? None is attached to the project yet.
2. **Real usage:** can you share sample SQL/PL/SQL from your tests, or the schema? It would sharpen the phase 1 and 2 scope.
3. ~~Ordering~~: decided. node-oracledb must connect to the running mock, so the wire protocol is phase 1.
4. **Name and license:** keep the name `mock-oracle`? Which license (MIT/Apache-2.0 is the Rust default)?
