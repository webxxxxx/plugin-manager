//! Upgrade endpoints (#264): `GET /api/v1/upgrade/check` and
//! `POST /api/v1/upgrade/apply`.
//!
//! Both run on the blocking pool, so the daemon's event loop keeps answering
//! the control requests the orchestration makes (`tools/call` to plugin-store,
//! `svcd.restart` afterwards).

use super::*;

impl ApiFace {
    pub(super) fn upgrade_check(&self, _call: &ApiCall<'_>) -> Response {
        match crate::upgrade::check(&self.config) {
            Ok(found) => Response::json(200, &found.to_json()),
            Err(error) => error_response(400, "upgrade_check_failed", &error),
        }
    }

    pub(super) fn upgrade_apply(&self, _call: &ApiCall<'_>) -> Response {
        match crate::upgrade::apply_remote(&self.config) {
            Ok(applied) => Response::json(200, &applied.to_json()),
            Err(error) => error_response(400, "upgrade_failed", &error),
        }
    }
}
