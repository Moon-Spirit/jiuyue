//! XP → level / rank-title progression (pure functions).
//!
//! The whole economy is a stateless table of integer XP and the two curves
//! the server and (in parallel, on the frontend) the clients both render:
//!
//! * **Level curve** — level 1→2 requires 50 XP; every further level
//!   requires the previous requirement × 1.1, rounded to the nearest whole
//!   number (`req(level→level+1) = round(50 × 1.1^(level-1))`, Rust f64
//!   `round` = half away from zero). Cumulative XP needed for level N is the
//!   sum of `req(1..N-1)`; `level_from_xp` is the monotone inverse.
//! * **Rank titles** — 5-level bands on a Minecraft material ladder, level
//!   1–5 土块, … 36–40 下界之星, 41+ 龙蛋. Levels continue past 45
//!   indefinitely (the XP curve keeps going; 41+ stays 龙蛋 forever).
//!
//! No I/O, no clock: every function here is a pure `fn` so server logic and
//! unit tests share one source of truth.

/// The per-message XP denominator: +10 XP per full 100 characters sent.
pub const MSG_XP_CHAR_BLOCK: usize = 100;
/// XP granted per full 100-character block of a sent plaintext message.
pub const MSG_XP_PER_BLOCK: i64 = 10;
/// Daily messaging XP cap (per user, per UTC day).
pub const MSG_XP_DAILY_CAP: i64 = 200;
/// Daily-login bonus, granted at most once per UTC day.
pub const DAILY_LOGIN_XP: i64 = 20;

/// The 1→2 XP requirement and geometric-curve base (`round(50 × 1.1^k)`).
const XP_REQ_BASE: i64 = 50;
const XP_REQ_GROWTH: f64 = 1.1;

/// Full rank-ladder rows: `(lowest level of the band, title)`. Bands are
/// strictly ascending and contiguous from level 1, so a level→title lookup is
/// a search for the last band whose floor the level reaches.
const RANK_TITLES: &[(i64, &str)] = &[
    (1, "土块"),
    (6, "木头"),
    (11, "石头"),
    (16, "铁块"),
    (21, "金块"),
    (26, "钻石块"),
    (31, "下界合金块"),
    (36, "下界之星"),
    (41, "龙蛋"),
];

/// `f64::powi` clone (exp ≥ 0) so the curve needs no `std` surprises and the
/// same exact multiplication order is reproducible in tests.
fn powi_growth(exp: u32) -> f64 {
    let mut result = 1.0f64;
    let mut base = XP_REQ_GROWTH;
    let mut exp = exp;
    while exp > 0 {
        if exp & 1 == 1 {
            result *= base;
        }
        base *= base;
        exp >>= 1;
    }
    result
}

/// XP required to advance from `level` to `level + 1` (level ≥ 1).
///
/// `req(1→2) = round(50 × 1.1^0) = 50`; every further level multiplies the
/// previous requirement by 1.1 before rounding (`round(50 × 1.1^(level-1))`).
pub fn xp_for_level(level: i64) -> i64 {
    debug_assert!(level >= 1);
    if level == 1 {
        return XP_REQ_BASE;
    }
    // round(50 × 1.1^(level-1)) with f64 round = half away from zero.
    (XP_REQ_BASE as f64 * powi_growth((level - 1) as u32)).round() as i64
}

/// Total XP required to REACH `level` (levels ≥ 2): the sum of
/// `req(1→2) … req((level-1)→level)`. Level 1 costs nothing.
pub fn total_xp_for(level: i64) -> i64 {
    debug_assert!(level >= 1);
    (1..level).map(xp_for_level).sum()
}

/// The highest level whose cumulative XP cost `total_xp` covers
/// (level 1 = 0 XP; strictly monotone in `xp`). Inverts [`total_xp_for`].
pub fn level_from_xp(total_xp: i64) -> i64 {
    let total_xp = total_xp.max(0);
    // Geometric-ish growth caps the search trivially: the cum sum reaches
    // i64::MAX-sized XP only after hundreds of levels, and each iteration is
    // O(1). Loop, don't solve a logarithm.
    let mut level = 1;
    let mut acc = 0i64;
    loop {
        let next_req = xp_for_level(level);
        let Some(next_acc) = acc.checked_add(next_req) else {
            return level;
        };
        if total_xp < next_acc {
            return level;
        }
        acc = next_acc;
        level += 1;
    }
}

/// Rank title for a level (Minecraft-material ladder, 5-level bands).
///
/// Level 1–5 土块 / 6–10 木头 / 11–15 石头 / 16–20 铁块 / 21–25 金块 /
/// 26–30 钻石块 / 31–35 下界合金块 / 36–40 下界之星 / 41+ 龙蛋. Every level
/// ≥ 41 — including levels far past 45, whose XP the curve keeps pricing —
/// carries the top title.
pub fn title_for_level(level: i64) -> &'static str {
    let level = level.max(1);
    RANK_TITLES
        .iter()
        .rev()
        .find(|(floor, _)| level >= *floor)
        .map(|(_, title)| *title)
        .expect("level >= 1 always matches the (1, ..) band")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_level_requirements_match_the_spec() {
        // req(l→l+1) = round(50 × 1.1^(l-1)), half away from zero.
        assert_eq!(xp_for_level(1), 50);
        assert_eq!(xp_for_level(2), 55);
        assert_eq!(xp_for_level(3), 61); // 50×1.21 = 60.5 → 61 (half up)
        assert_eq!(xp_for_level(4), 67);
        assert_eq!(xp_for_level(5), 73);
        assert_eq!(xp_for_level(6), 81);
        assert_eq!(xp_for_level(7), 89);
        assert_eq!(xp_for_level(8), 97);
        assert_eq!(xp_for_level(9), 107);
        assert_eq!(xp_for_level(10), 118);
        assert_eq!(xp_for_level(11), 130); // the .5-rounding needle: 50×1.1^10 = 129.6875
    }

    #[test]
    fn cumulative_xp_matches_the_sum_of_requirements() {
        assert_eq!(total_xp_for(1), 0);
        assert_eq!(total_xp_for(2), 50);
        assert_eq!(total_xp_for(3), 105);
        assert_eq!(total_xp_for(4), 166);
        assert_eq!(total_xp_for(5), 233);
        assert_eq!(total_xp_for(6), 306);
        assert_eq!(total_xp_for(7), 387);
        assert_eq!(total_xp_for(8), 476);
        assert_eq!(total_xp_for(9), 573);
        assert_eq!(total_xp_for(10), 680);
        assert_eq!(total_xp_for(11), 798);
    }

    #[test]
    fn level_boundaries_at_exact_costs() {
        // Each threshold XP lands exactly on the level whose cum cost it is.
        for level in 2..=12 {
            let xp = total_xp_for(level);
            assert_eq!(
                level_from_xp(xp),
                level,
                "exactly {xp} XP must be level {level}"
            );
        }
        assert_eq!(level_from_xp(0), 1, "0 XP is level 1");
        assert_eq!(
            level_from_xp(49),
            1,
            "one short of the 1→2 req stays level 1"
        );
    }

    #[test]
    fn level_is_monotone_in_xp() {
        for xp in [0, 1, 49, 50, 104, 105, 305, 306, 386, 387, 797, 798] {
            let level = level_from_xp(xp);
            let next = level_from_xp(xp + 1);
            assert!(next >= level, "level must never drop as xp grows");
        }
        // Spot-check interior monotonicity over a wide sweep.
        let mut prev = 0;
        for xp in (0..50_000).step_by(97) {
            let level = level_from_xp(xp);
            assert!(level >= prev, "level regressed at {xp}");
            prev = level;
        }
    }

    #[test]
    fn rank_title_bands_cover_every_level_without_gaps() {
        // Every band floor is hit exactly, and the inside of each band shares
        // the band title. Levels keep climbing past 45 (curve never stops).
        for (level, title) in [
            (1, "土块"),
            (5, "土块"),
            (6, "木头"),
            (10, "木头"),
            (11, "石头"),
            (15, "石头"),
            (16, "铁块"),
            (20, "铁块"),
            (21, "金块"),
            (25, "金块"),
            (26, "钻石块"),
            (30, "钻石块"),
            (31, "下界合金块"),
            (35, "下界合金块"),
            (36, "下界之星"),
            (40, "下界之星"),
            (41, "龙蛋"),
            (45, "龙蛋"),
            (46, "龙蛋"),
            (1000, "龙蛋"),
        ] {
            assert_eq!(title_for_level(level), title, "band at level {level}");
        }
        // 0 and below clamp to the 土块 floor.
        assert_eq!(title_for_level(0), "土块");
        assert_eq!(title_for_level(-3), "土块");
    }

    #[test]
    fn high_levels_stay_reachable_and_titled() {
        // Level 45 is comfortably inside the ladder and beyond any foreseeable
        // real play; the curve must still price it and the title stays top.
        let xp_45 = total_xp_for(45);
        assert_eq!(level_from_xp(xp_45), 45);
        assert_eq!(level_from_xp(xp_45 - 1), 44);
        assert_eq!(title_for_level(level_from_xp(xp_45)), "龙蛋");
        assert!(
            xp_for_level(45) > xp_for_level(44),
            "requirements keep growing"
        );
    }

    #[test]
    fn xp_constants_match_the_frozen_economy() {
        assert_eq!(MSG_XP_PER_BLOCK * 20, MSG_XP_DAILY_CAP, "200/day = 20 × 10");
        assert_eq!(DAILY_LOGIN_XP, 20);
        assert_eq!(MSG_XP_CHAR_BLOCK, 100);
    }
}
