/**
 * The one line a Typing Indicator shows, or `null` when nobody is typing.
 *
 * Pure and list-injected, so the group "who is typing" rendering is unit-tested
 * rather than inferred from a live socket. One Participant is named; up to
 * `maxNamed` Participants are all named; beyond that the line summarises by count,
 * because a 200-member Group would otherwise render a paragraph. Blank names are
 * dropped so a missing profile never produces a dangling separator.
 */
export function typingLabel(
  names: readonly string[],
  maxNamed = 2,
): string | null {
  const unique = [...new Set(names.filter((name) => name !== ""))];
  if (unique.length === 0) return null;
  if (unique.length === 1) return `${unique[0]} 正在输入…`;
  if (unique.length <= maxNamed) return `${unique.join("、")} 正在输入…`;
  return `${unique.length} 人正在输入…`;
}
