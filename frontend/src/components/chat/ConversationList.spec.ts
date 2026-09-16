import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import type { ConversationSummary } from "../../generated/ConversationSummary";
import ConversationList from "./ConversationList.vue";

const CONVERSATION_ID = "01JABC1234567890ABCDEFGHJ2";

/** A direct Conversation summary as the backend returns it. */
function conversation(): ConversationSummary {
  return {
    id: CONVERSATION_ID,
    kind: "direct",
    peer: {
      id: "01JABC1234567890ABCDEFGHJ3",
      username: "bob",
      display_name: "Bob",
      avatar_url: null,
    },
    unread_count: 0,
    created_at_ms: 1_700_000_000_000,
  };
}

function mountList(unreadCounts: Record<string, number>) {
  return mount(ConversationList, {
    props: {
      conversations: [conversation()],
      activeId: null,
      loading: false,
      unreadCounts,
    },
  });
}

describe("ConversationList", () => {
  it("renders the Unread Count as a badge", () => {
    const wrapper = mountList({ [CONVERSATION_ID]: 3 });

    expect(wrapper.get('[data-test="unread-badge"]').text()).toBe("3");
  });

  it("hides the badge when there is nothing unread", () => {
    const wrapper = mountList({ [CONVERSATION_ID]: 0 });

    expect(wrapper.find('[data-test="unread-badge"]').exists()).toBe(false);
  });

  it("caps a large count so the badge cannot stretch the row", () => {
    const wrapper = mountList({ [CONVERSATION_ID]: 150 });

    expect(wrapper.get('[data-test="unread-badge"]').text()).toBe("99+");
  });

  it("defaults a Conversation with no counted entry to no badge", () => {
    const wrapper = mountList({});

    expect(wrapper.find('[data-test="unread-badge"]').exists()).toBe(false);
  });
});
