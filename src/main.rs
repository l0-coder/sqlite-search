use clap::Parser;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;
use std::process;

const EXAMPLES: &str = "\
Examples:
  # 1. Search all tables and all fields (substring match, case-insensitive)
  sqlite-search app.db -q \"error\" -i

  # 2. Search specific fields (comma-separated or repeated -f) with exact match
  sqlite-search app.db -f code,status -q 218 --exact
  sqlite-search app.db -f code -f status -q 218 -e --full

  # 3. Restrict search to specific table(s) and limit output to 20 matches
  sqlite-search app.db -t users,logs -f message,email -q \"admin\" -i -n 20

  # 4. Include SQLite views in addition to tables
  sqlite-search app.db --views -q \"active\"";

#[derive(Parser, Debug)]
#[command(
    name = "sqlite-search",
    version,
    about = "Zero-dependency CLI tool to search values in SQLite (.db / .sqlite / .sqlite3) databases",
    after_help = EXAMPLES,
    arg_required_else_help = true
)]
struct Cli {
    /// Path to the SQLite database file
    #[arg(value_name = "FILE")]
    database: PathBuf,

    /// Value or string to search for
    #[arg(short = 'q', long = "query", value_name = "VALUE")]
    query: String,

    /// Field/column name(s) to search; omit to search all fields (repeatable or comma-separated)
    #[arg(short = 'f', long = "field", value_delimiter = ',', value_name = "FIELDS")]
    fields: Vec<String>,

    /// Restrict search to specific table(s); omit to search all user tables (repeatable or comma-separated)
    #[arg(short = 't', long = "table", value_delimiter = ',', value_name = "TABLES")]
    tables: Vec<String>,

    /// Also search SQLite views (default: tables only)
    #[arg(long = "views")]
    include_views: bool,

    /// Match the entire field value exactly (default: substring/contains match)
    #[arg(short = 'e', long = "exact")]
    exact: bool,

    /// Perform case-insensitive value matching (field and table names are always case-insensitive)
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Print all non-empty columns of each matched row
    #[arg(long = "full")]
    full: bool,

    /// Maximum number of matched rows to display (0 = unlimited)
    #[arg(short = 'n', long = "limit", default_value_t = 0, value_name = "COUNT")]
    limit: usize,
}

fn main() {
    let cli = Cli::parse();

    if let Err(err) = run(&cli) {
        eprintln!("Error: {err}");
        process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if !cli.database.exists() {
        return Err(format!("Database file not found: {}", cli.database.display()).into());
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&cli.database, flags)?;

    let list_sql = if cli.include_views {
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name"
    } else {
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    };

    let mut list_stmt = conn.prepare(list_sql)?;
    let all_tables: Vec<String> = list_stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let target_tables: Vec<String> = cli.tables.iter().map(|s| s.to_lowercase()).collect();
    let target_fields: Vec<String> = cli.fields.iter().map(|s| s.to_lowercase()).collect();
    let needle = if cli.ignore_case {
        cli.query.to_lowercase()
    } else {
        cli.query.clone()
    };

    let mut total_matches: usize = 0;
    let mut scanned_tables: usize = 0;

    for tbl_name in &all_tables {
        if !target_tables.is_empty() && !target_tables.contains(&tbl_name.to_lowercase()) {
            continue;
        }

        let query_sql = format!("SELECT * FROM {}", quote_ident(tbl_name));
        let mut stmt = match conn.prepare(&query_sql) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Warning: skipped [{}] (failed to prepare query): {}", tbl_name, e);
                continue;
            }
        };

        let columns: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        // Determine target column indices (0 fields specified -> scan all columns)
        let col_indices: Vec<usize> = if target_fields.is_empty() {
            (0..columns.len()).collect()
        } else {
            columns
                .iter()
                .enumerate()
                .filter(|(_, col)| target_fields.contains(&col.to_lowercase()))
                .map(|(idx, _)| idx)
                .collect()
        };

        // Skip scanning rows if none of the requested fields exist in this table
        if col_indices.is_empty() {
            continue;
        }

        scanned_tables += 1;

        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Warning: failed to query [{}]: {}", tbl_name, e);
                continue;
            }
        };

        let mut row_num: usize = 0;
        while let Some(row) = rows.next()? {
            row_num += 1;
            let mut row_hits: Vec<(&str, String)> = Vec::new();

            for &col_idx in &col_indices {
                let val_ref = match row.get_ref(col_idx) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(val_str) = value_ref_to_string(val_ref) else {
                    continue;
                };

                if is_match(&val_str, &needle, cli.exact, cli.ignore_case) {
                    row_hits.push((columns[col_idx].as_str(), val_str));
                }
            }

            if !row_hits.is_empty() {
                total_matches += 1;
                print!("[{}] Row #{}: ", tbl_name, row_num);
                let hit_summary: Vec<String> = row_hits
                    .iter()
                    .map(|(k, v)| format!("{k} = {v}"))
                    .collect();
                println!("{}", hit_summary.join(" | "));

                if cli.full {
                    let full_cols: Vec<String> = columns
                        .iter()
                        .enumerate()
                        .filter_map(|(i, col)| {
                            row.get_ref(i)
                                .ok()
                                .and_then(value_ref_to_string)
                                .map(|v| format!("{col}={v}"))
                        })
                        .collect();
                    println!("    Full row: {}", full_cols.join(" | "));
                }

                if cli.limit > 0 && total_matches >= cli.limit {
                    println!(
                        "\n--- Reached output limit ({}) after scanning {} table(s) ---",
                        cli.limit, scanned_tables
                    );
                    return Ok(());
                }
            }
        }
    }

    println!(
        "\n--- Search complete: scanned {} table(s), found {} match(es) ---",
        scanned_tables, total_matches
    );
    Ok(())
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn is_match(value: &str, needle: &str, exact: bool, ignore_case: bool) -> bool {
    if ignore_case {
        let lower = value.to_lowercase();
        if exact {
            lower == needle
        } else {
            lower.contains(needle)
        }
    } else if exact {
        value == needle
    } else {
        value.contains(needle)
    }
}

fn value_ref_to_string(val: ValueRef<'_>) -> Option<String> {
    match val {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Some(format!("<Blob: {} bytes>", bytes.len())),
    }
}