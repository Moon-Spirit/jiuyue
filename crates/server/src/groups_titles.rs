//! M12a group-local progression: group XP → level → title tiers, and the
//! display-title resolution rule (pure functions).
//!
//! This is the group analogue of [`jiuyue_domain::progress`]. The LEVEL curve
//! is deliberately NOT forked here — [`group_level_from_xp`] delegates to
//! `level_from_xp`, so a member's group level and their global level always
//! share one source of truth. Only the title ladder is group-specific (10
//! levels per tier, capped at 鞘翅 from level 91 up).
//!
//! Display title resolution (the server-side rule used by `GET /api/groups/{id}`):
//! a non-empty `custom_title` wins; otherwise a role label (`owner` → 群主,
//! `admin` → 管理员); otherwise the level tier title (plain members).

use jiuyue_domain::level_from_xp;

/// Group XP granted for each plaintext text message sent in a group.
pub const GROUP_XP_PER_MESSAGE: i64 = 10;
/// Daily group-XP cap per `(conversation, user)` pair, per UTC day.
pub const GROUP_XP_DAILY_CAP: i64 = 200;
/// Maximum length (characters) of an owner-set per-member custom title.
pub const CUSTOM_TITLE_MAX_CHARS: usize = 16;

/// Full group-title ladder as `(lowest level of the tier, title)`. Tiers are
/// strictly ascending and contiguous from level 1 and each spans 10 levels;
/// the final tier is open-ended (level 91+ stays 鞘翅).
const GROUP_TITLE_TIERS: &[(i64, &str)] = &[
    (1, "木头"),
    (11, "石头"),
    (21, "铁锭"),
    (31, "金锭"),
    (41, "钻石"),
    (51, "绿宝石"),
    (61, "下界合金锭"),
    (71, "下界之星"),
    (81, "龙蛋"),
    (91, "鞘翅"),
];

/// The group title for a group-local `level` (1-based, 10 levels per tier).
///
/// Level 1–10 木头, 11–20 石头, 21–30 铁锭, 31–40 金锭, 41–50 钻石,
/// 51–60 绿宝石, 61–70 下界合金锭, 71–80 下界之星, 81–90 龙蛋, 91+ 鞘翅.
/// Levels ≤ 0 clamp to the first tier.
pub fn title_for_group_level(level: i64) -> &'static str {
    let level = level.max(1);
    GROUP_TITLE_TIERS
        .iter()
        .rev()
        .find(|(floor, _)| level >= *floor)
        .map(|(_, title)| *title)
        .expect("level >= 1 always matches the (1, ..) tier")
}

/// Role label for group roles that render as a role instead of a tier.
fn role_label(role: &str) -> Option<&'static str> {
    match role {
        "owner" => Some("群主"),
        "admin" => Some("管理员"),
        _ => None,
    }
}

/// Resolves the DISPLAY title for a group member.
///
/// Precedence: a non-null, non-empty (after trim) `custom_title` wins;
/// otherwise `owner`/`admin` show their role label; otherwise the member's
/// level tier title. The raw stored `custom_title` is returned separately by
/// the API so clients can show an edit affordance.
pub fn resolve_member_title(custom_title: Option<&str>, role: &str, group_level: i64) -> String {
    if let Some(custom) = custom_title {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Some(label) = role_label(role) {
        return label.to_owned();
    }
    title_for_group_level(group_level).to_owned()
}

/// Group level for a group-local XP total. Reuses the global curve so the two
/// economies never diverge (same `req(L→L+1) = round(50 × 1.1^(L-1))`).
pub fn group_level_from_xp(group_xp: i64) -> i64 {
    level_from_xp(group_xp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiuyue_domain::{level_from_xp, total_xp_for};

    #[test]
    fn tier_boundaries_cover_every_level() {
        for (level, title) in [
            (0, "木头"),
            (1, "木头"),
            (10, "木头"),
            (11, "石头"),
            (20, "石头"),
            (21, "铁锭"),
            (30, "铁锭"),
            (31, "金锭"),
            (40, "金锭"),
            (41, "钻石"),
            (50, "钻石"),
            (51, "绿宝石"),
            (60, "绿宝石"),
            (61, "下界合金锭"),
            (70, "下界合金锭"),
            (71, "下界之星"),
            (80, "下界之星"),
            (81, "龙蛋"),
            (90, "龙蛋"),
            (91, "鞘翅"),
            (200, "鞘翅"),
            (1000, "鞘翅"),
        ] {
            assert_eq!(title_for_group_level(level), title, "tier at level {level}");
        }
    }

    #[test]
    fn negative_levels_clamp_to_the_first_tier() {
        assert_eq!(title_for_group_level(-1), "木头");
        assert_eq!(title_for_group_level(-100), "木头");
    }

    #[test]
    fn display_resolution_owner_and_admin_show_role_labels() {
        assert_eq!(resolve_member_title(None, "owner", 1), "群主");
        assert_eq!(resolve_member_title(None, "owner", 95), "群主");
        assert_eq!(resolve_member_title(None, "admin", 5), "管理员");
        assert_eq!(resolve_member_title(None, "admin", 95), "管理员");
    }

    #[test]
    fn display_resolution_members_show_tier_title() {
        assert_eq!(resolve_member_title(None, "member", 1), "木头");
        assert_eq!(resolve_member_title(None, "member", 11), "石头");
        assert_eq!(resolve_member_title(None, "member", 91), "鞘翅");
    }

    #[test]
    fn custom_title_overrides_both_role_and_tier() {
        // Rule: custom → role → tier. Custom wins even for owner/admin.
        assert_eq!(
            resolve_member_title(Some("矿工头子"), "member", 1),
            "矿工头子"
        );
        assert_eq!(
            resolve_member_title(Some("矿工头子"), "admin", 1),
            "矿工头子"
        );
        assert_eq!(
            resolve_member_title(Some("矿工头子"), "owner", 1),
            "矿工头子"
        );
    }

    #[test]
    fn blank_custom_title_falls_through_to_role_or_tier() {
        assert_eq!(resolve_member_title(Some(""), "admin", 1), "管理员");
        assert_eq!(resolve_member_title(Some("   "), "member", 11), "石头");
        assert_eq!(resolve_member_title(None, "member", 21), "铁锭");
    }

    #[test]
    fn group_level_reuses_the_domain_curve() {
        // +10/message ⇒ 5 messages reach exactly the level-2 threshold (50 XP).
        for level in 1..=12 {
            let xp = total_xp_for(level);
            assert_eq!(group_level_from_xp(xp), level);
            assert_eq!(group_level_from_xp(xp), level_from_xp(xp));
        }
        assert_eq!(group_level_from_xp(0), 1);
        assert_eq!(group_level_from_xp(49), 1);
        assert_eq!(group_level_from_xp(50), 2);
    }

    #[test]
    fn economy_constants_match_the_frozen_spec() {
        assert_eq!(GROUP_XP_PER_MESSAGE, 10);
        assert_eq!(GROUP_XP_DAILY_CAP, 200);
        assert_eq!(CUSTOM_TITLE_MAX_CHARS, 16);
    }
}
