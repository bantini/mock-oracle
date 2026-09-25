# mock-oracle

A slim, in-memory stand-in for Oracle Database in CI/CD. It runs Oracle SQL and PL/SQL, and node-oracledb connects to it unchanged.

> Status: early. node-oracledb (Thin mode, versions 6 and 7) connects, logs in and runs `SELECT … FROM DUAL` queries with expressions and bind variables. Tables, DML and PL/SQL come next; see [docs/design.md](docs/design.md) for the plan.

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

const db = await MockOracle.start();           // picks a free port; MockOracle.start({ port, password }) to choose
const conn = await oracledb.getConnection({ user: 'test', password: 'oracle', connectString: db.connectString });
const { rows } = await conn.execute('SELECT 1 FROM DUAL');   // [[1]]
await conn.close();
await db.stop();
```

## Use from Docker

```sh
docker build -t mock-oracle .
docker run -p 1521:1521 mock-oracle
# connect with any user name, password "oracle", connect string localhost:1521/FREEPDB1
```

## Logging in

Any user name and any service name are accepted. Every user shares one password: `oracle` by default. Change it with `MockOracle.start({ password })` or the `MOCK_ORACLE_PASSWORD` environment variable. Wrong passwords fail with ORA-01017, as on a real database.

## Develop

```sh
cargo test --workspace
cd crates/mock-oracle-node && npm install && npm run build:debug && npm test
```
