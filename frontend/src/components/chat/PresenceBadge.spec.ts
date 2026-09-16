import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import PresenceBadge from "./PresenceBadge.vue";

/** Mount the badge with a known presence. */
function mountBadge(
  status: "online" | "offline" | null,
  lastSeenMs: number | null,
) {
  return mount(PresenceBadge, { props: { status, lastSeenMs } });
}

describe("PresenceBadge", () => {
  it("shows nothing at all when the presence is unknown", () => {
    const wrapper = mountBadge(null, null);

    expect(wrapper.find('[data-test="presence"]').exists()).toBe(false);
  });

  it("shows 在线 for a reachable User", () => {
    const wrapper = mountBadge("online", null);

    expect(wrapper.get('[data-test="presence-label"]').text()).toBe("在线");
  });

  it("shows the last-seen time for an offline User", () => {
    const wrapper = mountBadge("offline", Date.now() - 5 * 60_000);

    expect(wrapper.get('[data-test="presence-label"]').text()).toBe(
      "最后在线 5 分钟前",
    );
  });

  it("shows 离线 for an offline User who has never been seen", () => {
    const wrapper = mountBadge("offline", null);

    expect(wrapper.get('[data-test="presence-label"]').text()).toBe("离线");
  });
});
