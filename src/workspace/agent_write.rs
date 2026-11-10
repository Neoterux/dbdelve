//! An approved write.

use crate::db::{ConnectionConfig, Engine, QueryResult};

pub(crate) struct Writing {}

pub(crate) const WRITE_TIMEOUT: u32 = 600;

pub(crate) fn write_timeout(user: u32) -> u32 {
    user
}

pub(crate) fn agent_write_config(config: &ConnectionConfig) -> ConnectionConfig {
    config.clone()
}

pub(super) fn written_text(_engine: Engine, _result: &QueryResult) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Column, every_shape};

    #[test]
    fn the_write_config_carries_the_write_bound() {
        assert_eq!(write_timeout(0), WRITE_TIMEOUT);
        assert_eq!(write_timeout(5), 5);
        assert_eq!(write_timeout(u32::MAX), WRITE_TIMEOUT);
        for profile in every_shape() {
            for user in [0, 5, WRITE_TIMEOUT, WRITE_TIMEOUT + 1] {
                let profile = profile.clone().with_statement_timeout(user);
                let opened = agent_write_config(&profile);
                assert_eq!(
                    opened.statement_timeout(),
                    write_timeout(user),
                    "{:?} {user}",
                    profile.engine()
                );
                assert_eq!(
                    opened.with_statement_timeout(user),
                    profile,
                    "only the timeout differs"
                );
            }
        }
    }

    #[test]
    fn an_approved_write_answers_its_count_or_that_it_ran() {
        let counted = |rows_affected| QueryResult {
            rows_affected,
            ..QueryResult::default()
        };
        for (affected, text) in [
            (Some(1), "The server reported 1 row affected."),
            (Some(3), "The server reported 3 rows affected."),
            (None, "The statement ran."),
        ] {
            assert_eq!(written_text(Engine::Sqlite, &counted(affected)), text);
        }

        let returned = QueryResult {
            columns: vec![Column {
                name: "id".to_string(),
                data_type: None,
            }],
            ..QueryResult::default()
        };
        let text = written_text(Engine::Sqlite, &returned);
        assert!(text.contains("id"), "{text}");
        assert!(!text.contains("affected"), "{text}");
    }
}
