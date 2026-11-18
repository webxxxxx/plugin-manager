//! 对账的纯判定：合并期望集、找出该遗忘的 id、决定每个定义的动作。

use std::collections::BTreeMap;

use crate::kernel::registry::{Kind, Service};

/// 一次对账里该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// 注册表里没有：登记并把状态机放进 starting。
    Add,
    /// 注册表里有：替换定义，运行状态沿用旧的。
    Update,
    /// 两边都是插件：install/uninstall RPC 才是权威，扫描不覆盖。
    Skip,
}

/// Core 定义优先；插件扫描只补 core 里没有的 id。
pub(crate) fn merge_desired(
    core: impl IntoIterator<Item = Service>,
    plugins: Vec<Service>,
) -> BTreeMap<String, Service> {
    let mut desired: BTreeMap<String, Service> =
        core.into_iter().map(|service| (service.id.clone(), service)).collect();
    for service in plugins {
        if desired.contains_key(&service.id) {
            continue;
        }
        desired.insert(service.id.clone(), service);
    }
    desired
}

/// 注册表里有、这次期望集里没有的 id：要停并遗忘。
pub(crate) fn stale_ids<'a>(
    current: impl IntoIterator<Item = &'a str>,
    desired: &BTreeMap<String, Service>,
) -> Vec<String> {
    current
        .into_iter()
        .filter(|id| !desired.contains_key(*id))
        .map(str::to_string)
        .collect()
}

/// 一个期望定义落到注册表上是什么动作。
pub(crate) fn action_for(existing: Option<&Service>, desired: &Service) -> Action {
    match existing {
        None => Action::Add,
        Some(old) if old.kind == Kind::Plugin && desired.kind == Kind::Plugin => Action::Skip,
        Some(_) => Action::Update,
    }
}
