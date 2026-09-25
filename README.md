# mock-oracle

A slim, in-memory stand-in for Oracle Database in CI/CD. It runs Oracle SQL and PL/SQL, and node-oracledb connects to it unchanged.

> Status: early skeleton. The server accepts connections but does not speak the Oracle protocol yet. See [docs/design.md](docs/design.md) for the plan.

## Layout

| Crate | What it is |
|---|---|
| `crates/mock-oracle-core` | The engine: SQL/PL/SQL parser, executor, in-memory storage. No I/O. |
| `crates/mock-oracle-server` | TNS listener for thin drivers; also the Docker image. |
| `crates/mock-oracle-node` | npm package `mock-oracle` (napi-rs). Starts the server inside your Node process. |

## Use from Node (planned API)

```js
const { MockOracle } = require('mock-oracle');
const oracledb = require('oracledb');

const db = await MockOracle.start();           // picks a free port
const conn = await oracledb.getConnection({ user: 'test', password: 'test', connectString: db.connectString });
// ...
await db.stop();
```

## Use from Docker

```sh
docker build -t mock-oracle .
docker run -p 1521:1521 mock-oracle
```

## Develop

```sh
cargo test --workspace
cd crates/mock-oracle-node && npm install && npm run build:debug
```
