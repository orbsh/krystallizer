//! 应用表：一个应用一行。
//!
//! 频道行的 `app_id` 是指向这里的值，不是一段随手写的字符串。这样「这个应用有哪些频道」
//! 是一次索引扫描（`by_app`），而不是把每条频道读出来比对字段；应用自己以后要挂的配置
//! （模型、限额、展示名）也有地方放，不会挤进频道行。

use okm_core::{DocumentEncode, KeyEncode, Uuid7};

/// 应用的键：OIDC `client_id` 的值，部署时写定，永不改标。
#[derive(KeyEncode, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct AppKey {
    pub app_id: Uuid7,
}

/// 一行应用。`name` 只是给人看的标签，改它不影响任何过滤。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(AppKey)]
#[ok_ns(43)]
#[ok_name("apps")]
pub struct AppRow {
    pub name: String,
    pub created_ms: i64,
}
