# mock-oracle

A slim, in-memory stand-in for Oracle Database in CI/CD pipelines. It speaks Oracle's network protocol, so node-oracledb connects to it unchanged, and it runs Oracle SQL and PL/SQL against tables held in memory. It starts in milliseconds, needs no Oracle installation or license, and forgets everything when it stops.

You can use it in two ways:

- **As an npm package** that starts the mock inside your Node test process. Nothing to run beside your tests.
- **As a Docker image** that listens on port 1521 like a real database, for pipelines, docker compose, or clients in any language that use a thin Oracle driver.

> Status: early. node-oracledb (Thin mode, versions 6 and 7) connects and runs SQL against in-memory tables (DDL, DML, queries with joins and grouping, transactions, dates), sequences, `RETURNING … INTO`, and the common parts of PL/SQL: anonymous blocks and stored procedures and functions with IN, OUT and IN OUT binds. See [What works](#what-works) for details and [docs/design.md](docs/design.md) for the plan.

**Contents:** [Install the npm package](#install-the-npm-package) · [Use the npm package](#use-the-npm-package) · [Run the Docker image](#run-the-docker-image) · [Use the image in CI](#use-the-docker-image-in-ci) · [Connecting](#connecting) · [What works](#what-works) · [Develop](#develop) · [Release](#release) · [License](#license)

## Install the npm package

Requirements: Node.js 18 or newer, and [node-oracledb](https://www.npmjs.com/package/oracledb) 6 or 7 in its default Thin mode.

```sh
npm install --save-dev mock-oracle
npm install oracledb          # if your project does not have it already
```

The package ships prebuilt binaries for:

| OS | Architectures |
|---|---|
| Linux (glibc 2.17 or newer: Debian, Ubuntu, RHEL, Amazon Linux, …) | x64, arm64 |
| Linux (musl: Alpine) | x64, arm64 |
| macOS | Intel, Apple silicon |
| Windows | x64 |

Installing needs no Rust toolchain, Oracle client or Docker.

## Use the npm package

### Start, connect, stop

```js
const { MockOracle } = require('mock-oracle');
const oracledb = require('oracledb');

const db = await MockOracle.start({
  seed: `
    create table dept (id number(4) primary key, name varchar2(30) not null);
    insert into dept values (10, 'Sales');
  `,
});

const conn = await oracledb.getConnection({
  user: 'test',                      // any user name works
  password: 'oracle',                // the default password
  connectString: db.connectString,   // e.g. "localhost:40213/FREEPDB1"
});
const { rows } = await conn.execute('select name from dept');   // [['Sales']]

await conn.close();
await db.stop();
```

Each `MockOracle.start()` is an independent database on its own port, so parallel test files do not see each other's data. Connection pools (`oracledb.createPool`) work the same way.

### Options

`MockOracle.start(options)` takes:

| Option | Default | Meaning |
|---|---|---|
| `port` | `0` | Port to listen on. `0` picks a free port; read it back from `db.connectString`. |
| `password` | `"oracle"` | Password every user logs in with. A wrong password fails with ORA-01017. |
| `seed` | none | SQL to run before the first connection, such as `CREATE TABLE` and `INSERT` statements. |

In `seed` and `runScript`, end SQL statements with `;`. End each PL/SQL block, procedure or function with a line holding only `/`, as in SQL\*Plus.

### Methods

| Member | What it does |
|---|---|
| `db.connectString` | Easy Connect string for node-oracledb: `localhost:<port>/FREEPDB1`. |
| `db.runScript(sql)` | Runs SQL statements without a connection and commits them. |
| `db.snapshot()` | Saves the committed contents of every table and sequence. |
| `db.restore(snapshot?)` | Puts the data back as it was in `snapshot`, or right after start and seeding when no snapshot is given. |
| `db.stop()` | Stops the server. Returns a promise. |

### In a test suite

Start the mock once per test file, seed it with your schema, and reset it before each test so tests do not depend on each other. This example uses Node's built-in test runner; Jest and Mocha look the same with `beforeAll`/`afterAll` or `before`/`after`.

```js
const { test, before, after, beforeEach } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const { MockOracle } = require('mock-oracle');
const oracledb = require('oracledb');

let db, conn;

before(async () => {
  db = await MockOracle.start({ seed: fs.readFileSync('test/schema.sql', 'utf8') });
  conn = await oracledb.getConnection({ user: 'app', password: 'oracle', connectString: db.connectString });
});

beforeEach(async () => {
  await conn.rollback();   // drop anything a failed test left uncommitted
  db.restore();            // back to the seeded data
});

after(async () => {
  await conn?.close();
  await db?.stop();
});

test('adds a department', async () => {
  await conn.execute("insert into dept values (20, 'Ops')");
  await conn.commit();
  const { rows } = await conn.execute('select count(*) from dept');
  assert.deepStrictEqual(rows, [[2]]);
});
```

If your application code reads its connection settings from the environment, point it at the mock before loading it, for example `process.env.DB_CONNECT_STRING = db.connectString`.

TypeScript types are included.

## Run the Docker image

The image is `ghcr.io/bantini/mock-oracle`, built for `linux/amd64` and `linux/arm64`.

```sh
docker run --rm -p 1521:1521 ghcr.io/bantini/mock-oracle:0.1
```

Then connect with any user name, password `oracle`, and connect string `localhost:1521/FREEPDB1`.

```js
const conn = await oracledb.getConnection({ user: 'test', password: 'oracle', connectString: 'localhost:1521/FREEPDB1' });
```

### Tags

| Tag | Points to |
|---|---|
| `0.1.0` | Exactly that release. |
| `0.1` | The newest `0.1.x` release. Recommended for pipelines. |
| `latest` | The newest release. |

### Load a schema and data

At startup the server runs every `.sql` file in `/docker-entrypoint-initdb.d`, in name order, just like the official Oracle and Postgres images. Mount a folder of scripts there:

```sh
docker run --rm -p 1521:1521 -v "$PWD/sql:/docker-entrypoint-initdb.d:ro" ghcr.io/bantini/mock-oracle:0.1
```

```text
sql/
  01-schema.sql     create table …;
  02-data.sql       insert into …;
  03-procs.sql      create or replace procedure … end;
                    /
```

The same rules as `seed` apply: `;` ends a SQL statement, and a line holding only `/` ends a PL/SQL block. If a script fails, the container exits and prints the file and the ORA- error. Data lives in memory only, so restarting the container gives you the seeded state again.

### Settings

| Environment variable | Default | Meaning |
|---|---|---|
| `MOCK_ORACLE_PASSWORD` | `oracle` | Password every user logs in with. |
| `MOCK_ORACLE_SEED` | `/docker-entrypoint-initdb.d` | A `.sql` file, or a folder of them, to run at startup. |
| `MOCK_ORACLE_ADDR` | `0.0.0.0:1521` | Address and port to listen on inside the container. |
| `RUST_LOG` | `info` | Log level: `error`, `warn`, `info`, `debug` or `trace`. |

To expose it on another host port, map it: `-p 1600:1521` and connect to `localhost:1600/FREEPDB1`.

### docker compose

```yaml
services:
  oracle:
    image: ghcr.io/bantini/mock-oracle:0.1
    ports: ['1521:1521']
    environment:
      MOCK_ORACLE_PASSWORD: secret
    volumes:
      - ./sql:/docker-entrypoint-initdb.d:ro
  app:
    build: .
    environment:
      DB_CONNECT_STRING: oracle:1521/FREEPDB1
    depends_on: [oracle]
```

Other containers on the same network reach it by service name, here `oracle:1521/FREEPDB1`.

## Use the Docker image in CI

The mock is ready as soon as the container starts, so no health-check wait is needed.

**GitHub Actions**

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      oracle:
        image: ghcr.io/bantini/mock-oracle:0.1
        ports: ['1521:1521']
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 22 }
      - run: npm ci
      - run: npm test
        env:
          DB_CONNECT_STRING: localhost:1521/FREEPDB1
```

Service containers cannot mount files from the checkout, so to seed from your repository start the container in a step instead:

```yaml
      - run: docker run -d -p 1521:1521 -v "$PWD/sql:/docker-entrypoint-initdb.d:ro" ghcr.io/bantini/mock-oracle:0.1
```

**GitLab CI**

```yaml
test:
  image: node:22
  services:
    - name: ghcr.io/bantini/mock-oracle:0.1
      alias: oracle
  variables:
    DB_CONNECT_STRING: oracle:1521/FREEPDB1
  script:
    - npm ci
    - npm test
```

If your tests are written in Node, the npm package is usually simpler than the image in CI: it needs no service container, and each test file gets its own database.

## Connecting

- **User:** any user name is accepted.
- **Password:** `oracle`, unless you set `password` or `MOCK_ORACLE_PASSWORD`. Every user shares it.
- **Service name:** any name is accepted. `FREEPDB1` matches Oracle Database Free, so the same connect string works against a real database too.
- **Connect string:** Easy Connect, `host:port/service`.
- **Drivers:** node-oracledb 6 and 7 in Thin mode are tested. Don't call `oracledb.initOracleClient()`, because Thick mode goes through Oracle's client libraries and is not supported. Other thin drivers, such as python-oracledb in Thin mode or JDBC Thin, speak the same protocol but aren't tested yet.

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

## Layout

| Crate | What it is |
|---|---|
| `crates/mock-oracle-core` | The engine: SQL/PL/SQL parser, executor, in-memory storage. No I/O. |
| `crates/mock-oracle-server` | TNS listener for thin drivers; also the Docker image. |
| `crates/mock-oracle-node` | npm package `mock-oracle` (napi-rs). Starts the server inside your Node process. |

## Develop

```sh
cargo test --workspace
cd crates/mock-oracle-node && npm install && npm run build:debug && npm test
```

## Release

1. Set the same version in `Cargo.toml` (`[workspace.package]`) and `crates/mock-oracle-node/package.json`, and merge that to `main`.
2. Tag it and push the tag: `git tag v0.1.0 && git push origin v0.1.0`.

The [Release workflow](.github/workflows/release.yml) then builds and tests the native binaries on every platform, pushes the Docker image to `ghcr.io/bantini/mock-oracle`, publishes `mock-oracle` to npm, and creates a GitHub release. Publishing to npm needs an npm automation token in the repository secret `NPM_TOKEN`. The first time the image is pushed, GitHub creates its package as private. Make it public once under the package's settings (Danger Zone, Change visibility) so pipelines can pull it without logging in; the workflow warns while it is still private. A version with a suffix such as `0.2.0-rc.1` is published to npm under the `next` tag and does not move the `latest` Docker tag.

## License

[MIT](LICENSE)
