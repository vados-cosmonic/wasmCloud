# 🐘 SQL Database Postgres Provider

This capability provider implements the [wasmcloud:sqldb-postgres][wasmcloud-sqldb-postgres-wit] WIT package, which enables SQL-driven database interaction with a [Postgres][postgres] database cluster.

This provider handles concurrent component connections, and components can choose how they manage connections:

| Management type    | Description                                                       |
|--------------------|-------------------------------------------------------------------|
| Completely Managed | Defined at provider startup via host-supplied named configuration |
| Link Driven        | Defined via config for each component-provider link               |
| Component Driven   | Controlled completely by connected components                     |

This means that components can either use a simple interface to perform requests, relying on pre-established configuration for determining what database/connection/session information to use, *or*  create their own connections, managed by the provider.

Want to read all the functionality included the interface? [Start from `provider.wit`](./wit/provider.wit) to read what this provider can do, and work your way to [`types.wit`](./wit/types.wit).

Note that connections are local to a single provider, so multiple providers running on the same lattice will *not* share connections automatically.

[postgres]: https://postgresql.org
[wasmcloud-sqldb-postgres-wit]: https://github.com/vados-cosmonic/wit-wasmcloud-postgres

## 👟 Quickstart

To get this provider started quickly, you can start with:

```console
wash start provider ghcr.io/wasmcloud/provider-sqldb-postgres:0.1.0
```

The easiest way to start a Postgres provider with configuration specified, and a component that uses it is with [wasmCloud Application Deployment Manager][wadm]. 

<details>
<summary>Example manifest for an HTTP server with a database connection</summary>

```yaml
apiVersion: core.oam.dev/v1beta1
kind: Application
metadata:
  name: sqldb-postgres-example
  annotations:
    version: v0.0.1
    description:  SQLDB Postgres example
spec:
  components:
    # A capability provider that enables Postgres access for the component
    - name: sqldb-postgres
      type: capability
      properties:
        image: ghcr.io/wasmcloud/sqldb-postgres:0.1.0
        config: 
          # NOTE: Since this is a named configuration provided by the host,
          # you'll need to make sure it exists *before* deploying this manifest!
          #
          # see: `wash config put`
          - name: default-postgres

    # A capability provider that provides HTTP serving for the component
    - name: http-server
      type: capability
      properties:
        image: ghcr.io/wasmcloud/http-server:0.20.0

    # A component that uses both capability providers above (HTTP server and sqldb-postgres)
    # to provide a TODO app on http://localhost:8080
    - name: todo-app
      type: component
      properties:
        image: ghcr.io/wasmcloud/component-todoapp-postgres-rust:0.1.0
      traits:
        # Govern the spread/scheduling of the component
        - type: spreadscaler
          properties:
            replicas: 1

        # Link the httpserver to the component, and configure the HTTP server
        # to listen on port 8080 for incoming requests
        - type: link
          properties:
            target: http-server
            namespace: wasi
            package: http
            interfaces: [incoming-handler]
            source_config:
              - name: default-http
                properties:
                  address: 127.0.0.1:8080

        # Link the sqldb-provider to the component, specifying the postgres cluster URL
        - type: link
          properties:
            target: sqldb-postgres
            namespace: wasmcloud
            package: sqldb-postgres
            interfaces: [migrations, managed-raw, managed-prepared]
            target_config:
              - name: pg
                properties:
                  url: postgres://127.0.0.1:5432
```

</details>

[wadm]: https://github.com/wasmCloud/wadm


## 📑 Named configuration Settings

As connection details are considered sensitive information, they should be specified via named config available to the provider *at startup*, rather than via link definitions.

> ![NOTE]
> Named configuration can be specified by:
>
> - Using appropriate options on `wash config put`

| Property                      | Example                     | Description                                                                |
|-------------------------------|-----------------------------|----------------------------------------------------------------------------|
| `MANAGED_URL`                 | `postgres://localhost:5432` | URL to use for all managed connections (overrides `*_HOST`, `*_PORT`, etc) |
| `MANAGED_HOST`                | `localhost`                 | Hostname for all managed connections                                       |
| `MANAGED_PORT`                | `5432`                      | Port for all managed connections                                           |
| `MANAGED_USERNAME`            | `postgres`                  | Username for all managed connections                                       |
| `MANAGED_PASSWORD`            | `postgres`                  | Password for all managed connections                                       |
| `MANAGED_TLS_REQUIRED`        | `false`                     | Whether TLS should be required for al managed connections                  |
| `PROFILE_<name>_URL`          | `postgres://some-db:5432`   | URL to use for all named profile connection `<name>`                       |
| `PROFILE_<name>_HOST`         | `localhost`                 | Hostname to use for named profile connection `<name>`                      |
| `PROFILE_<name>_PORT`         | `5432`                      | Port to use for named profile connection `<name>`                          |
| `PROFILE_<name>_USERNAME`     | `postgres`                  | Username to use for named profile connection `<name>`                      |
| `PROFILE_<name>_PASSWORD`     | `postgres`                  | Password to use for named profile connection `<name>`                      |
| `PROFILE_<name>_TLS_REQUIRED` | `false`                     | Whether to require TLS for named profile connection `<name>`               |

> ![NOTE]
> As multiple components will connect to the same provider, the primary method of configuring connection "profiles" is via ENV variables
> that are prefixed with `PROFILE_<name of profile>`.
>
> For example, to create a minimal working profile named `demo`, the named configuration value `PROFILE_demo_URL=localhost:5432` is all you need

If you have many profiles, you may want to contain them in a single JSON file available at a secure (possibly bindmounted) location on disk, and present that path to the provider via the named configuration variable `PROFILES_JSON_DIR_PATH`. Only the *first* level of files beneath the directory will be processed.

An example of a minimally valid JSON file representing a profile (ex. `demo.json`):

```json
{
  "url": "localhost:5432"
}
```

## 🔗 Link Definition Configuration Settings

The following is a list of configuration settings you can specify on *each link* (i.e. commonly `target_config`in a WADM manifest) for this provider.

> ![NOTE]
> Link definition configuration can be specified by:
>
> - Writing a WADM manifest with proper `link` traits (with `source_config`/`target_config` values)
> - Using appropriate options on `wash link put`

| Property             | Example | Description                                        |
|----------------------|---------|----------------------------------------------------|
| `CONNECTION_PROFILE` | `demo`  | Name of the connection profile that should be used |

# 🛠️ Development

## 📦 Building a PAR

To build a [Provider Archive (`.par`/`.par.gz`)][par] for this provider, first build the project with `wash`:

```console
wash build
```

Then run `wash par`:

```
wash par create \
  --compress
  --binary target/debug/sqldb-postgres-provider
  --vendor wasmcloud
  --version 0.1.0
  --name sqldb-postgres-provider`
```

[par]: https://wasmcloud.com/docs/developer/providers/build
