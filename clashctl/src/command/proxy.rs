use std::time::Duration;

use clap::{Parser, Subcommand};
use clashctl_core::{
    model::{Proxies, ProxyType},
    strum::VariantNames,
    Clash,
};
use log::{error, info, warn};
use owo_colors::OwoColorize;
use rayon::prelude::*;
use requestty::{prompt_one, Answer, ListItem, Question};
use terminal_size::{terminal_size, Height, Width};

use crate::{
    interactive::{Flags, ProxySortBy, SortOrder},
    RenderList, Result,
};
// use crate::{Result};

// #[allow(clippy::match_str_case_mismatch)]
// impl FromStr for ProxyType {
//     type Err = Error;
//     fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
//         match s.to_ascii_lowercase().as_str() {
//             "direct" => Ok(Self::Direct),
//             "reject" => Ok(Self::Reject),
//             "selector" => Ok(Self::Selector),
//             "urltest" => Ok(Self::URLTest),
//             "fallback" => Ok(Self::Fallback),
//             "loadbalance" => Ok(Self::LoadBalance),
//             "shadowsocks" => Ok(Self::Shadowsocks),
//             "vmess" => Ok(Self::Vmess),
//             "ssr" => Ok(Self::ShadowsocksR),
//             "http" => Ok(Self::Http),
//             "snell" => Ok(Self::Snell),
//             "trojan" => Ok(Self::Trojan),
//             "socks5" => Ok(Self::Socks5),
//             "relay" => Ok(Self::Relay),
//             _ => Err(Error::BadOption)
//         }
//     }
// }

#[derive(Subcommand, Debug)]
#[clap(about = "Interacting with proxies")]
pub enum ProxySubcommand {
    #[clap(alias = "ls", about = "List proxies (alias ls)")]
    List(ProxyListOpt),
    #[clap(about = "Set active proxy")]
    Use,
    #[clap(about = "Test proxy delay")]
    Test,
}

#[derive(Parser, Debug, Clone)]
pub struct ProxyListOpt {
    #[clap(
        long,
        default_value = "delay",
        possible_values = &["type", "name", "delay"],
    )]
    pub sort_by: ProxySortBy,

    #[clap(
        long,
        default_value = "ascendant",
        possible_values = &["ascendant", "descendant"],
    )]
    pub sort_order: SortOrder,

    #[clap(short, long, help = "Reverse the listed result")]
    pub reverse: bool,

    #[clap(
        short,
        long,
        help = "Exclude proxy types",
        conflicts_with = "include",
        possible_values = ProxyType::VARIANTS
    )]
    pub exclude: Vec<ProxyType>,

    #[clap(
        short,
        long,
        help = "Include proxy types",
        conflicts_with = "exclude",
        possible_values = ProxyType::VARIANTS
    )]
    pub include: Vec<ProxyType>,

    #[clap(short, long, help = "Show proxies and groups without cascading")]
    pub plain: bool,
}

impl ProxySubcommand {
    pub fn handle(&self, flags: &Flags) -> Result<()> {
        let config = flags.get_config()?;
        let server = match config.using_server() {
            Some(server) => server.to_owned(),
            None => {
                warn!("No server configured yet. Use `clashctl server add` first.");
                return Ok(());
            }
        };
        info!("Using {}", server);
        let clash = server.into_clash_with_timeout(Some(Duration::from_millis(flags.timeout)))?;

        match self {
            ProxySubcommand::List(opt) => {
                let proxies = clash.get_proxies()?;
                proxies.render_list(opt);
            }
            ProxySubcommand::Use => {
                let proxies = clash.get_proxies()?;
                let group_selected = select_proxy_group(&proxies, "Which group to change?")?;
                let proxy = clash.get_proxy(&group_selected)?;

                // all / now only occurs when proxy_type is [`ProxyType::Selector`]
                let members = proxy.all.unwrap();
                let now = proxy.now.unwrap();
                let cur_index = members.iter().position(|x| x == &now).unwrap();
                let mut question = Question::select("proxy")
                    .message("Which proxy to use?")
                    .choices(members);
                if cur_index != 0 {
                    question = question.default(cur_index)
                }
                let member_selected = match prompt_one(question.build()) {
                    Ok(result) => match result {
                        Answer::ListItem(ListItem { text, .. }) => text,
                        _ => unreachable!(),
                    },
                    Err(e) => {
                        error!("Error selecting proxy: {}", e);
                        return Err(e.into());
                    }
                };
                info!(
                    "Setting group {} to use {}",
                    group_selected.green(),
                    member_selected.green()
                );
                clash.set_proxygroup_selected(&group_selected, &member_selected)?;
                info!("Done!")
            }
            ProxySubcommand::Test => {
                let proxies = clash.get_proxies()?;
                let group_selected = select_proxy_group(&proxies, "Which group to test?")?;
                let proxy = clash.get_proxy(&group_selected)?;
                let members = proxy.all.unwrap();
                let delays = test_proxy_delays(&clash, flags, &members);
                let results = order_proxy_test_results(&members, delays);
                let choices = build_proxy_test_choices(&results, proxy_test_name_width());

                let member_selected = select_proxy_with_delays(choices, &results)?;
                info!(
                    "Setting group {} to use {}",
                    group_selected.green(),
                    member_selected.green()
                );
                clash.set_proxygroup_selected(&group_selected, &member_selected)?;
                info!("Done!")
            }
        }
        Ok(())
    }
}

fn select_proxy_group(proxies: &Proxies, message: &str) -> Result<String> {
    let mut groups = proxies
        .iter()
        .filter(|(_, p)| p.proxy_type.is_selector())
        .map(|(name, _)| name)
        .filter(|name| !["GLOBAL", "REJECT"].contains(&name.as_str()))
        .collect::<Vec<_>>();
    groups.sort();

    match prompt_one(
        Question::select("proxy")
            .message(message)
            .choices(groups)
            .build(),
    ) {
        Ok(result) => Ok(result.as_list_item().unwrap().text.to_owned()),
        Err(e) => {
            error!("Error selecting proxy: {}", e);
            Err(e.into())
        }
    }
}

fn test_proxy_delays(
    clash: &Clash,
    flags: &Flags,
    members: &[String],
) -> Vec<(String, Option<u64>)> {
    members
        .par_iter()
        .map(|member| {
            let delay = clash
                .get_proxy_delay(member, flags.test_url.as_str(), flags.timeout)
                .ok()
                .map(|result| result.delay);
            (member.to_owned(), delay)
        })
        .collect()
}

fn select_proxy_with_delays(
    choices: Vec<String>,
    results: &[(String, Option<u64>)],
) -> Result<String> {
    match prompt_one(
        Question::select("proxy")
            .message("Which proxy to use?")
            .choices(choices)
            .build(),
    ) {
        Ok(result) => match result {
            Answer::ListItem(ListItem { index, .. }) => Ok(results[index].0.to_owned()),
            _ => unreachable!(),
        },
        Err(e) => {
            error!("Error selecting proxy: {}", e);
            Err(e.into())
        }
    }
}

fn order_proxy_test_results(
    members: &[String],
    delays: Vec<(String, Option<u64>)>,
) -> Vec<(String, Option<u64>)> {
    members
        .iter()
        .filter_map(|member| {
            delays
                .iter()
                .find(|(name, _)| name == member)
                .map(|(_, delay)| (member.to_owned(), *delay))
        })
        .collect()
}

fn build_proxy_test_choices(
    results: &[(String, Option<u64>)],
    name_width: usize,
) -> Vec<String> {
    results
        .iter()
        .map(|(name, delay)| {
            format!(
                "{:<1$}  {2}",
                truncate_proxy_name(name, name_width),
                name_width,
                format_proxy_delay(*delay)
            )
        })
        .collect()
}

fn proxy_test_name_width() -> usize {
    let (Width(terminal_width), _) = terminal_size().unwrap_or((Width(70), Height(0)));
    usize::from(terminal_width).saturating_sub(18).max(20)
}

fn truncate_proxy_name(name: &str, width: usize) -> String {
    let char_count = name.chars().count();
    if char_count <= width {
        return name.to_owned();
    }

    let keep = width.saturating_sub(1);
    let mut truncated = name.chars().take(keep).collect::<String>();
    truncated.push('…');
    truncated
}

fn format_proxy_delay(delay: Option<u64>) -> String {
    match delay {
        Some(delay) => format!("{}ms", delay),
        None => "timeout".to_owned(),
    }
}

#[test]
fn test_proxy_type() {
    let string = "direct";
    let parsed = string.parse().unwrap();
    assert_eq!(ProxyType::Direct, parsed);
}

#[test]
fn test_proxy_delay_label() {
    assert_eq!(format_proxy_delay(Some(123)), "123ms");
    assert_eq!(format_proxy_delay(None), "timeout");
}

#[test]
fn test_proxy_test_result_keeps_group_order() {
    let members = vec!["node-b".to_owned(), "node-a".to_owned()];
    let delays = vec![("node-a".to_owned(), Some(20)), ("node-b".to_owned(), None)];

    let results = order_proxy_test_results(&members, delays);

    assert_eq!(
        results,
        vec![("node-b".to_owned(), None), ("node-a".to_owned(), Some(20))]
    );
}

#[test]
fn test_proxy_test_choices_show_delay_on_right() {
    let results = vec![
        ("node-a".to_owned(), Some(123)),
        ("node-b".to_owned(), None),
    ];

    let choices = build_proxy_test_choices(&results, 42);

    assert_eq!(choices[0], format!("{:<42}  {}", "node-a", "123ms"));
    assert_eq!(choices[1], format!("{:<42}  {}", "node-b", "timeout"));
}

#[test]
fn test_proxy_test_choices_truncate_long_name_before_delay() {
    let results = vec![(
        "justmysocks-JMS-1327608@c3s1.portablesubmarines.com:11063".to_owned(),
        None,
    )];

    let choices = build_proxy_test_choices(&results, 24);

    assert_eq!(choices[0], "justmysocks-JMS-1327608…  timeout");
}

#[test]
fn test_truncate_proxy_name_keeps_short_name() {
    assert_eq!(truncate_proxy_name("node-a", 24), "node-a");
}

#[test]
fn test_truncate_proxy_name_uses_ellipsis_for_long_name() {
    assert_eq!(truncate_proxy_name("abcdefghijklmnopqrstuvwxyz", 8), "abcdefg…");
}
