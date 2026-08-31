//! 多源堆叠气泡的纯逻辑(无平台依赖,headless 可测):哪些接入口出卡、
//! 出现/消失滞后、稳定排序与前排卡归属。
//!
//! 设计要点(见行为设计):
//! - 仅 Working/Thinking 生效;Done/Failed/Attention 聚合气泡维持现状。
//! - 每个有活动会话(working ∪ thinking)的接入口一张卡;出现滞后
//!   [`APPEAR_LAG_MS`](会话闪现/健康抖动不闪卡)、消失滞后 [`GONE_LAG_MS`]
//!   (衔接两个会话的间隙)。
//! - 排序稳定:非前排卡按状态权重(attention > failed > done >
//!   working = thinking,同级)再按接入口注册序;前排卡固定末位。
//!   绝不按"最近活跃"排序,杜绝洗牌。
//! - 上限 [`MAX_CARDS`] 张(350 + 3×44 ≈ 482px,装得进可用高度),
//!   前排卡在截断时保住。

use crate::bubble_text::BubbleText;
use crate::state::{Snapshot, Source};
use std::collections::BTreeMap;

/// 卡片出现滞后:源持续有活动会话超过该时长才出卡(ms)。
pub const APPEAR_LAG_MS: u64 = 1500;
/// 卡片消失滞后:源停止活跃后卡片再停留该时长(ms)。
pub const GONE_LAG_MS: u64 = 1500;
/// 堆叠卡上限。窗口不长大:350 + 3×44 ≈ 482px。
pub const MAX_CARDS: usize = 4;

/// 一张堆叠卡的布局判定结果(纯数据,GUI 据此 layout/draw)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackCard {
    /// 接入口注册序(Source::Script(id))。
    pub id: u16,
    /// 前排卡(完整流式内容,画在最下/最后);其余卡只露头部。
    pub front: bool,
    /// 该源当前有 working 会话(逐卡纠正标题/着色;否则按 thinking)。
    pub working: bool,
}

/// 每源的出现/消失滞后计时(按源 id 存于调用方)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StackState {
    /// 源持续有活动会话的起点(None = 当前不活跃)。
    pub active_since: Option<u64>,
    /// 源停止活跃后的滞留起点(None = 不在滞留期)。
    pub gone_since: Option<u64>,
}

/// 源的状态权重(排序键,越大越靠前):working = thinking = 1 <
/// done = 2 < failed = 3 < attention = 4。常规堆叠(全局 Working/Thinking)
/// 下所有卡同级(=1),排序退化为接入口注册序 —— 顺序稳定,绝不洗牌。
/// attention 为全局聚合态(待确认时全局模式即 Attention,不出堆叠卡),
/// 这里仅作防御性兜底,不会在实际排序中出现。
fn state_rank(snap: &Snapshot, id: u16) -> u8 {
    let src = Source::Script(id);
    if snap.working.iter().chain(&snap.thinking).any(|s| s.source == src) {
        1
    } else if snap.done.iter().any(|s| s.source == src) {
        2
    } else if snap.failed.iter().any(|s| s.source == src) {
        3
    } else {
        4
    }
}

/// 该源当前是否有活动会话(working ∪ thinking 里存在它的会话)。
fn has_sessions(snap: &Snapshot, id: u16) -> bool {
    let src = Source::Script(id);
    snap.working.iter().chain(&snap.thinking).any(|s| s.source == src)
}

fn has_working(snap: &Snapshot, id: u16) -> bool {
    let src = Source::Script(id);
    snap.working.iter().any(|s| s.source == src)
}

/// 计算当前应显示的堆叠卡(后→前顺序:末位 = 前排卡)。
///
/// - `front_session`:轮流机制(rotate_pick)选中的前排会话 id;其所在源
///   的卡为前排卡(完整流式内容)。None/会话已消失 = 无前排卡。
/// - `now`:当前毫秒(与 Snapshot 生成同源)。
/// - `state`:每源滞后计时,跨 tick 保留(调用方持有)。
pub fn stack_cards(
    snap: &Snapshot,
    front_session: Option<&str>,
    now: u64,
    state: &mut BTreeMap<u16, StackState>,
) -> Vec<StackCard> {
    // 1) 候选源 = 有活动会话的源 ∪ 仍在滞留期的源(按注册序)
    let mut ids: Vec<u16> = Vec::new();
    for s in snap.working.iter().chain(&snap.thinking) {
        let Source::Script(id) = s.source;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for k in state.keys() {
        if !ids.contains(k) {
            ids.push(*k);
        }
    }
    ids.sort_unstable();

    // 2) 出现/消失滞后判定
    let mut out: Vec<StackCard> = Vec::new();
    for id in ids {
        if has_sessions(snap, id) {
            let st = state.entry(id).or_default();
            st.active_since.get_or_insert(now);
            st.gone_since = None;
            let appeared = st
                .active_since
                .map(|t| now.saturating_sub(t) >= APPEAR_LAG_MS)
                .unwrap_or(false);
            if appeared {
                out.push(StackCard { id, front: false, working: has_working(snap, id) });
            }
        } else if let Some(st) = state.get_mut(&id) {
            // 源停止活跃:卡片再停留 GONE_LAG_MS(衔接会话间隙),到期移除
            match st.gone_since {
                None => st.gone_since = Some(now),
                Some(g) if now.saturating_sub(g) >= GONE_LAG_MS => {
                    state.remove(&id);
                    continue;
                }
                Some(_) => {}
            }
            let appeared = st
                .active_since
                .map(|t| now.saturating_sub(t) >= APPEAR_LAG_MS)
                .unwrap_or(false);
            if appeared {
                out.push(StackCard { id, front: false, working: has_working(snap, id) });
            } else {
                // 从未出过卡(闪现 < 出现滞后):没有可"衔接"的间隙,立即清理,
                // 两次闪现也不会累积成一次出卡
                state.remove(&id);
            }
        }
    }

    // 3) 前排归属:rotate_pick 选中会话所在的源
    let front_id: Option<u16> = front_session.and_then(|sid| {
        snap.working
            .iter()
            .chain(&snap.thinking)
            .find(|s| s.session_id == sid)
            .map(|s| match s.source {
                Source::Script(i) => i,
            })
    });

    // 4) 排序:非前排在前(状态权重降序 → 注册序升序),前排固定末位
    out.sort_by(|a, b| {
        let (fa, fb) = (Some(a.id) == front_id, Some(b.id) == front_id);
        fa.cmp(&fb)
            .then_with(|| state_rank(snap, b.id).cmp(&state_rank(snap, a.id)))
            .then_with(|| a.id.cmp(&b.id))
    });
    for c in &mut out {
        c.front = Some(c.id) == front_id;
    }

    // 5) 截断到上限:前排卡(末位)必须保住 —— 有前排时只截中间多余的
    //    非前排卡,无前排时按注册序保留前 MAX_CARDS 张
    if out.len() > MAX_CARDS {
        if front_id.is_some_and(|f| out.iter().any(|c| c.id == f)) {
            out.drain(MAX_CARDS - 1..out.len() - 1);
        } else {
            out.truncate(MAX_CARDS);
        }
    }
    out
}

/// 每卡标题:源在 working 列表有会话 → "正在干活…",否则 → "思考中…"。
/// 全局 mode 只反映最高优先级(所有源共用一个状态),堆叠卡需逐源纠正。
pub fn card_title(snap: &Snapshot, source: Source) -> String {
    if snap.working.iter().any(|s| s.source == source) {
        "正在干活…".to_string()
    } else {
        "思考中…".to_string()
    }
}

/// 就地把一张卡的 BubbleText 标题纠正为该源自己的状态标题
/// (bubble_text_pinned 的标题来自全局 mode,对堆叠卡不可靠)。
pub fn fix_card_title(text: &mut BubbleText, snap: &Snapshot, source: Source) {
    text.title = card_title(snap, source);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LiveText, SessionInfo};

    fn snap(mode: crate::state::Mode) -> Snapshot {
        Snapshot { mode, ..Default::default() }
    }

    fn sess(id: &str, src: Source) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            source: src,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        }
    }

    fn working(s: &mut Snapshot, id: &str, src: Source) {
        s.working.push(sess(id, src));
    }

    fn thinking(s: &mut Snapshot, id: &str, src: Source) {
        s.thinking.push(sess(id, src));
    }

    fn ids(cards: &[StackCard]) -> Vec<u16> {
        cards.iter().map(|c| c.id).collect()
    }

    /// 预热:在 t=0 见到所有会话(启动出现滞后计时),之后在 t=9000 断言。
    fn warm(s: &Snapshot, front: Option<&str>, st: &mut BTreeMap<u16, StackState>) {
        stack_cards(s, front, 0, st);
    }

    #[test]
    fn card_appears_only_after_appear_lag() {
        let mut s = snap(crate::state::Mode::Working);
        working(&mut s, "d1", Source::Script(0));
        let mut st = BTreeMap::new();
        // 首次见到会话即开始计时(t=0),闪现 <1.5s:不出卡
        assert!(stack_cards(&s, Some("d1"), 0, &mut st).is_empty());
        assert!(stack_cards(&s, Some("d1"), 1499, &mut st).is_empty());
        // 持续 ≥1.5s:出卡
        let cards = stack_cards(&s, Some("d1"), 1500, &mut st);
        assert_eq!(ids(&cards), vec![0]);
        assert!(cards[0].front, "唯一一张卡即前排(单源退化为现状)");
    }

    #[test]
    fn card_stays_through_gone_lag_then_removed() {
        let mut s = snap(crate::state::Mode::Working);
        working(&mut s, "d1", Source::Script(0));
        let mut st = BTreeMap::new();
        stack_cards(&s, Some("d1"), 0, &mut st);
        assert_eq!(ids(&stack_cards(&s, Some("d1"), 1500, &mut st)), vec![0]);
        // 会话消失:卡片在滞留期内保留(gone_since=2500)
        s.working.clear();
        assert_eq!(ids(&stack_cards(&s, None, 2500, &mut st)), vec![0]);
        assert_eq!(ids(&stack_cards(&s, None, 3999, &mut st)), vec![0]);
        // 滞留期结束(2500 + 1500):移除
        assert!(stack_cards(&s, None, 4000, &mut st).is_empty());
        // 会话闪现但从未达到出现滞后:不滞留、直接移除(闪现不闪卡)
        let mut st2 = BTreeMap::new();
        working(&mut s, "d2", Source::Script(1));
        stack_cards(&s, None, 5000, &mut st2); // active_since=5000(<1.5s 未出卡)
        s.working.clear();
        assert!(stack_cards(&s, None, 5100, &mut st2).is_empty());
        assert!(st2.is_empty());
    }

    #[test]
    fn reactivation_within_gone_lag_keeps_card_seamless() {
        let mut s = snap(crate::state::Mode::Working);
        working(&mut s, "d1", Source::Script(0));
        let mut st = BTreeMap::new();
        stack_cards(&s, Some("d1"), 0, &mut st);
        stack_cards(&s, Some("d1"), 1500, &mut st); // 卡已出现
        // 消失 1s(未到期)后新会话接上:卡不消失,也不重新计出现滞后
        s.working.clear();
        stack_cards(&s, None, 2500, &mut st);
        working(&mut s, "d2", Source::Script(0));
        assert_eq!(ids(&stack_cards(&s, Some("d2"), 2600, &mut st)), vec![0]);
    }

    #[test]
    fn order_is_registration_index_front_pinned_last() {
        let mut s = snap(crate::state::Mode::Working);
        // 注册序 2,0,1(列表顺序无关,按 id 排)
        working(&mut s, "h1", Source::Script(2));
        working(&mut s, "d1", Source::Script(0));
        working(&mut s, "m1", Source::Script(1));
        let mut st = BTreeMap::new();
        warm(&s, Some("d1"), &mut st);
        // 前排 = 源 0:末位;非前排按注册序(1,2)在前
        let cards = stack_cards(&s, Some("d1"), 9000, &mut st);
        assert_eq!(ids(&cards), vec![1, 2, 0]);
        assert!(!cards[0].front && !cards[1].front && cards[2].front);
        // 前排换到源 2:仍固定末位,非前排保持注册序(不按最近活跃洗牌)
        let cards = stack_cards(&s, Some("h1"), 9100, &mut st);
        assert_eq!(ids(&cards), vec![0, 1, 2]);
        assert!(cards[2].front);
        // 无前排(会话消失):全部按注册序
        let cards = stack_cards(&s, None, 9200, &mut st);
        assert_eq!(ids(&cards), vec![0, 1, 2]);
        assert!(cards.iter().all(|c| !c.front));
    }

    #[test]
    fn front_follows_rotate_pick_session() {
        let mut s = snap(crate::state::Mode::Working);
        working(&mut s, "d1", Source::Script(0));
        thinking(&mut s, "h1", Source::Script(1));
        let mut st = BTreeMap::new();
        warm(&s, Some("h1"), &mut st);
        // thinking 会话也能当前排(rotate_pick 跨 working ∪ thinking)
        let cards = stack_cards(&s, Some("h1"), 9000, &mut st);
        assert_eq!(ids(&cards), vec![0, 1]);
        assert!(cards[1].front);
        // thinking 卡的标题应为"思考中…"(逐源纠正,全局 mode 是 Working)
        assert_eq!(card_title(&s, Source::Script(1)), "思考中…");
        assert_eq!(card_title(&s, Source::Script(0)), "正在干活…");
    }

    #[test]
    fn single_source_degenerates_to_one_front_card() {
        let mut s = snap(crate::state::Mode::Thinking);
        thinking(&mut s, "h1", Source::Script(0));
        let mut st = BTreeMap::new();
        warm(&s, Some("h1"), &mut st);
        let cards = stack_cards(&s, Some("h1"), 9000, &mut st);
        assert_eq!(cards.len(), 1);
        assert!(cards[0].front);
        assert!(!cards[0].working, "thinking 源 working=false(标题/着色用)");
    }

    #[test]
    fn more_than_max_cards_truncated_keeping_front() {
        let mut s = snap(crate::state::Mode::Working);
        for i in 0..6u16 {
            working(&mut s, &format!("s{i}"), Source::Script(i));
        }
        let mut st = BTreeMap::new();
        warm(&s, Some("s4"), &mut st);
        // 前排 = 源 4:截断后仍保留(末位),其余取注册序最前的 3 张
        let cards = stack_cards(&s, Some("s4"), 9000, &mut st);
        assert_eq!(cards.len(), MAX_CARDS);
        assert_eq!(ids(&cards), vec![0, 1, 2, 4]);
        assert!(cards[3].front);
        // 无前排:取注册序最前的 4 张
        let cards = stack_cards(&s, None, 9000, &mut st);
        assert_eq!(ids(&cards), vec![0, 1, 2, 3]);
    }

    #[test]
    fn state_rank_orders_done_above_working_but_keeps_stable() {
        let mut s = snap(crate::state::Mode::Working);
        working(&mut s, "d1", Source::Script(2)); // 纯 working
        // 源 1:working 会话 + done 会话(完成窗口未关) → 权重 2,排在纯 working 前
        working(&mut s, "m1", Source::Script(1));
        s.done.push(sess("fin1", Source::Script(1)));
        let mut st = BTreeMap::new();
        warm(&s, None, &mut st);
        let cards = stack_cards(&s, None, 9000, &mut st);
        assert_eq!(ids(&cards), vec![1, 2]);
        // 同权重源之间仍按注册序(working = thinking 同级)
        let mut s2 = snap(crate::state::Mode::Working);
        working(&mut s2, "d1", Source::Script(1));
        thinking(&mut s2, "h1", Source::Script(0));
        let mut st2 = BTreeMap::new();
        warm(&s2, None, &mut st2);
        assert_eq!(ids(&stack_cards(&s2, None, 9000, &mut st2)), vec![0, 1]);
    }

    #[test]
    fn fix_card_title_rewrites_global_mode_title() {
        let mut s = snap(crate::state::Mode::Working);
        thinking(&mut s, "h1", Source::Script(1));
        working(&mut s, "d1", Source::Script(0));
        // 全局 Working:bubble_text_pinned 对源 1 的标题是"正在干活…",需纠正
        let mut t = crate::bubble_text::bubble_text_pinned(&s, Some(Source::Script(1)), None, None, 120);
        assert_eq!(t.title, "正在干活…");
        fix_card_title(&mut t, &s, Source::Script(1));
        assert_eq!(t.title, "思考中…");
        let mut t = crate::bubble_text::bubble_text_pinned(&s, Some(Source::Script(0)), None, None, 120);
        fix_card_title(&mut t, &s, Source::Script(0));
        assert_eq!(t.title, "正在干活…");
    }
}
