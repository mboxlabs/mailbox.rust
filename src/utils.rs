//! 地址解析 / 规范化，以及只读快照（peek）的选项归一与副本。
//!
//! 三端（TS `src/lib/utils.ts` / Python `mboxlabs/mailbox/utils.py` / Rust）用同一条地址规则，
//! 参数名保持一致：**邮箱语义留在 path 里**，query / hash 不参与寻址，因此带着它们的地址直接被拒绝。

use url::Url;

use crate::error::{MailboxError, Result};

/// 校验一个地址在规范化时不会**静默丢掉**信息。
///
/// `mem:inbox@host/thread?thread=1` 和 `mem:inbox@host/thread` 是两个逻辑地址，但它们的
/// canonical id 相同（见 [`get_canonical_mailbox_address_identifier`]）—— query / hash 会被丢掉。
/// 与其让两条互不相干的业务流共用同一条队列（issue#4：「看起来能用、实际共用一条队列」），
/// 不如在入口就拒绝：**邮箱语义必须留在 path 里**，query / hash 不参与寻址。
///
/// `url` crate 把空 query（`mem:host/inbox?`）保留成 `Some("")`，空 hash 同理 —— 所以
/// `is_some()` 就够覆盖「带空 `?` / 空 `#`」这两种写法，不必像 TS / Python 那样回读原始字符串。
///
/// @param address 待校验的地址。
/// @param context 出错信息里带上的调用点名字（例如 `"post()"`），便于定位。
/// @returns `Err(InvalidMailboxAddress)`：地址带 query 或 hash（含空的那些）。
pub fn assert_lossless_mailbox_address(address: &Url, context: &str) -> Result<()> {
    let mut dropped: Vec<String> = Vec::new();

    if let Some(query) = address.query() {
        if query.is_empty() {
            dropped.push("an empty query (\"?\")".to_string());
        } else {
            dropped.push(format!("query \"?{}\"", query));
        }
    }

    if let Some(hash) = address.fragment() {
        if hash.is_empty() {
            dropped.push("an empty hash (\"#\")".to_string());
        } else {
            dropped.push(format!("hash \"#{}\"", hash));
        }
    }

    if dropped.is_empty() {
        return Ok(());
    }

    Err(MailboxError::invalid_mailbox_address(
        address.as_str(),
        format!(
            "Invalid mailbox address \"{}\" in {}: {} would be dropped by address canonicalization, \
             silently merging this address with \"{}\". Mailbox semantics must live in the path \
             (e.g. \"mem:host/inbox/thread-1\"), not in the query or hash.",
            address.as_str(),
            context,
            dropped.join(" and "),
            canonical_of(address),
        ),
    ))
}

/// 把用户传入的地址字符串解析成 [`Url`]，并校验它可被无损规范化
/// （见 [`assert_lossless_mailbox_address`]）。
///
/// 所有拿字符串的入口都走这里，保证「同一个地址在每个入口都指向同一个 topic」，
/// 且非法形式在同一处报错。已经解析过的（`"mem:..." .parse::<Url>()`）走
/// [`assert_lossless_mailbox_address`]。
///
/// @param input 地址字符串。
/// @param context 出错信息里带上的调用点名字。
/// @returns 解析后的 `Url`。
/// @returns `Err(InvalidMailboxAddress)`：无法解析，或带 query / hash。
pub fn parse_mailbox_address(input: &str, context: &str) -> Result<Url> {
    let address = Url::parse(input).map_err(|error| {
        MailboxError::invalid_mailbox_address(
            input,
            format!(
                "Invalid mailbox address \"{}\" in {}: not a parseable URL ({}).",
                input, context, error
            ),
        )
    })?;

    assert_lossless_mailbox_address(&address, context)?;
    Ok(address)
}

/// 生成地址的 canonical id（topic 键）：`protocol://user@host[:port]/path`，不带 query / hash，
/// 抹掉密码 —— 与 TS / Python 端同一形状。
///
/// query 曾经也被「顺手丢掉」：那会让 `?thread=1`、`#fragment` 这类写法看起来能用、实际和
/// 不带 query 的地址塌成同一个 topic（issue#4）。现在这类地址在规范化之前就被拒绝，调用方
/// 拿到的是明确错误，而不是一条被合并的队列。
///
/// canonical id 带上了 protocol 与端口，因此可以再解析回地址（`Mailbox::snapshot()` 就是这么按
/// canonical id 复查每个地址的），并且规范化是幂等的。
///
/// @param address 待规范化的地址。
/// @returns canonical id；地址带 query / hash 时返回 `Err(InvalidMailboxAddress)`。
pub fn get_canonical_mailbox_address_identifier(address: &Url) -> Result<String> {
    assert_lossless_mailbox_address(address, "address canonicalization")?;
    Ok(canonical_of(address))
}

/// 由已解析的地址拼出 canonical id，**不**做无损校验（错误信息里要用它指出「会跟谁合并」）。
fn canonical_of(address: &Url) -> String {
    let scheme = address.scheme();
    let path = address.path();

    match address.host_str() {
        Some(host) => {
            // 端口属于地址的一部分（TS 的 `url.host` 含端口）：漏掉它会让
            // `mem://a@example.com:8080/inbox` 与 `mem://a@example.com/inbox` 塌成同一个 topic。
            let host = match address.port() {
                Some(port) => format!("{}:{}", host, port),
                None => host.to_string(),
            };
            let user = address.username();
            if user.is_empty() {
                format!("{}://{}{}", scheme, host, path)
            } else {
                // 密码（`url.password()`）不进 topic：它不是地址的一部分。
                format!("{}://{}@{}{}", scheme, user, host, path)
            }
        }
        // opaque 形式（`mem:user@host/path`）：整段都是 path。补上 `//` 后与 hierarchical 形式
        // （`mem://user@host/path`）落到同一个 topic，并且能再解析回来。
        None => format!("{}://{}", scheme, path),
    }
}

/// 校验并归一 `peek` 的 `limit`：`None` 表示「整个队列」。
///
/// TS / Python 要在这里拒绝负数、小数、`NaN`；Rust 的类型系统（`usize`）已经在编译期排除了
/// 它们，所以这里只是把三端的调用点对齐，并保留同一个落点 —— 公开类型一旦放宽，校验只改这里。
pub fn normalize_peek_limit(limit: Option<usize>) -> Option<usize> {
    limit
}

/// 给只读快照（peek）做一份消息副本。
///
/// 副本是必需的：直接把队列里的对象交给调用方，调用方改一下字段就等于改了队列内容，
/// 「只读」就成了空话。
///
/// 拷贝强度比 TS / Python 更好，也写清楚：Rust 里队列持有的是**owned** 值，
/// [`Clone`] 即深拷贝（`Url` / `serde_json::Value` / `HashMap` 都是），所以 peek 的隔离是绝对的，
/// 不存在 TS / Python 那种「body 里带函数就退化成共享引用」的情况。代价是大 body 会整份复制 ——
/// peek 是诊断路径，不为它做写时复制。
pub fn copy_for_peek<T: Clone>(message: &T) -> T {
    message.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MailboxError;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("测试里的地址必须可解析")
    }

    fn canon(raw: &str) -> String {
        get_canonical_mailbox_address_identifier(&url(raw)).expect("canonical id")
    }

    #[test]
    fn canonical_id_is_protocol_user_host_path() {
        // 端口属于地址；密码不属于。三端同一个形状。
        assert_eq!(
            canon("mem://bob@example.com:8080/inbox"),
            "mem://bob@example.com:8080/inbox"
        );
        assert_eq!(canon("mem://bob:secret@example.com/inbox"), "mem://bob@example.com/inbox");
        assert_eq!(canon("mem:inbox"), "mem://inbox");
    }

    #[test]
    fn a_different_port_is_a_different_topic() {
        // 旧实现漏掉端口：两个不同的服务会落到同一条队列。
        assert_ne!(
            canon("mem://bob@example.com:8080/inbox"),
            canon("mem://bob@example.com:9090/inbox")
        );
    }

    #[test]
    fn opaque_and_hierarchical_forms_land_on_the_same_topic() {
        assert_eq!(canon("mem:alice@example.com/inbox"), canon("mem://alice@example.com/inbox"));
    }

    #[test]
    fn canonical_id_is_idempotent_and_can_be_queried_back() {
        let once = canon("mem://alice@example.com/inbox/thread-1");
        assert_eq!(canon(&once), once);

        // `Mailbox::snapshot()` 要靠 canonical id 复查地址。
        let reparsed = parse_mailbox_address(&once, "snapshot()").unwrap();
        assert_eq!(get_canonical_mailbox_address_identifier(&reparsed).unwrap(), once);
    }

    #[test]
    fn query_is_rejected_instead_of_trimmed_off() {
        let error = get_canonical_mailbox_address_identifier(&url("mem:q@example.com/inbox?thread=1"))
            .unwrap_err();
        assert!(
            matches!(error, MailboxError::InvalidMailboxAddress { .. }),
            "query 必须报错，不是被裁掉：{:?}",
            error
        );
        assert!(error.to_string().contains("would be dropped by address canonicalization"));

        // 塌缩的直接后果：query 版与裸地址版 canonical id 相同 —— 必须无法发生。
        assert_eq!(canon("mem:q@example.com/inbox"), "mem://q@example.com/inbox");
    }

    #[test]
    fn hash_is_rejected_and_the_message_names_the_dropped_part() {
        let error = assert_lossless_mailbox_address(&url("mem:h@example.com/inbox#frag"), "status()")
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("hash \"#frag\" would be dropped"), "{}", message);
        assert!(message.contains("in status()"), "{}", message);
    }

    #[test]
    fn query_and_hash_together_are_both_reported() {
        let message = assert_lossless_mailbox_address(&url("mem:a@example.com/i?x=1#f"), "mailbox address")
            .unwrap_err()
            .to_string();
        assert!(message.contains("query \"?x=1\" and hash \"#f\""), "{}", message);
    }

    #[test]
    fn empty_query_and_empty_hash_count_too() {
        // 写法不同、意图相同：都会被规范化丢掉，所以一并拒绝。
        let with_query = assert_lossless_mailbox_address(&url("mem:a@example.com/i?"), "fetch()")
            .unwrap_err()
            .to_string();
        let with_hash = assert_lossless_mailbox_address(&url("mem:a@example.com/i#"), "fetch()")
            .unwrap_err()
            .to_string();
        assert!(with_query.contains("empty query (\"?\")"), "{}", with_query);
        assert!(with_hash.contains("empty hash (\"#\")"), "{}", with_hash);
    }

    #[test]
    fn percent_encoded_markers_are_path_content_and_stay_legal() {
        // `%3F` / `%23` 是 path 的内容，不是分隔符：合法，且原样留在 canonical id 里。
        assert_lossless_mailbox_address(&url("mem:a@example.com/inbox%3Fthread%3D1%23frag"), "post()")
            .unwrap();
        assert_eq!(
            canon("mem:a@example.com/inbox%3Fthread%3D1"),
            "mem://a@example.com/inbox%3Fthread%3D1"
        );
    }

    #[test]
    fn error_carries_the_address_and_a_stable_code() {
        let error = parse_mailbox_address("mem:q@example.com/inbox?thread=1", "post()").unwrap_err();
        match &error {
            MailboxError::InvalidMailboxAddress { address, .. } => {
                assert_eq!(address, "mem:q@example.com/inbox?thread=1");
            }
            other => panic!("期望 InvalidMailboxAddress，得到 {:?}", other),
        }
        assert_eq!(error.code(), Some("MBX_INVALID_ADDRESS"));

        // 错误信息要能指出「会跟谁合并」，否则调用方看不懂自己撞了哪条队列。
        let message = error.to_string();
        assert!(message.contains("mem:q@example.com/inbox?thread=1"), "{}", message);
        assert!(message.contains("mem://q@example.com/inbox"), "{}", message);
        assert!(message.contains("in post()"), "{}", message);
    }

    #[test]
    fn unparseable_input_becomes_an_address_error() {
        for raw in ["not a mailbox address", "inbox/thread", ""] {
            let error = parse_mailbox_address(raw, "subscribe()").unwrap_err();
            let message = error.to_string();
            assert!(message.contains("not a parseable URL"), "{}", message);
            assert!(message.contains("in subscribe()"), "{}", message);
            assert_eq!(error.code(), Some("MBX_INVALID_ADDRESS"));
        }
    }

    #[test]
    fn parse_keeps_the_spelling_and_resolves_the_provider_protocol() {
        let parsed = parse_mailbox_address("mem:a@example.com/inbox", "subscribe()").unwrap();
        assert_eq!(parsed.scheme(), "mem");
        assert_eq!(
            get_canonical_mailbox_address_identifier(&parsed).unwrap(),
            canon("mem://a@example.com/inbox")
        );
    }
}
