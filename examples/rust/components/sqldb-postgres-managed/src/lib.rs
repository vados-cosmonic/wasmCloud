wit_bindgen::generate!();

// NOTE: the imports below is generated by wit_bindgen::generate, due to the
// WIT definition(s) specified in `wit`
use wasmcloud::postgres::managed_query::{self, PgValue};

// NOTE: the `Guest` trait corresponds to the export of the `invoke` interface,
// namespaced to the current WIT namespace & package ("wasmcloud:examples")
use exports::wasmcloud::examples::invoke::Guest;

/// This struct must implement the all `export`ed functionality
/// in the WIT definition (see `wit/component.wit`)
struct QueryRunner;

const CREATE_TABLE_QUERY: &str = r#"
CREATE TABLE IF NOT EXISTS managed_example (
  id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
  description text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT NOW()
);
"#;

/// A basic insert query, using Postgres `RETURNING` syntax,
/// which returns the contents of the row that was inserted
const INSERT_QUERY: &str = r#"
INSERT INTO managed_example (description) VALUES ($1) RETURNING *;
"#;

impl Guest for QueryRunner {
    fn call() -> String {
        // First, ensure the right table is present
        if let Err(e) = managed_query::query(CREATE_TABLE_QUERY, &[]) {
            return format!("ERROR - failed to create table: {e}");
        };

        // Perform a managed query -- the connection details are managed
        // entirely by the provider (via MANAGED_*) named config
        match managed_query::query(
            INSERT_QUERY,
            &[PgValue::Text(format!("inserted example row!"))],
        ) {
            Ok(rows) => format!("SUCCESS: inserted new row:\n{rows:#?}"),
            Err(e) => format!("ERROR: failed to insert row: {e}"),
        }
    }
}

export!(QueryRunner);
