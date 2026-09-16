import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import CreateGroupForm from "./CreateGroupForm.vue";

function mountForm() {
  return mount(CreateGroupForm, { props: { busy: false } });
}

describe("CreateGroupForm", () => {
  it("keeps the submit disabled until a title and two members are given", async () => {
    const wrapper = mountForm();
    const submit = wrapper.get('[data-test="create-group-submit"]');

    expect(submit.attributes("disabled")).toBeDefined();

    await wrapper.get('[data-test="group-title-input"]').setValue("九月小组");
    await wrapper.get('[data-test="group-members-input"]').setValue("bob");
    expect(submit.attributes("disabled")).toBeDefined();

    await wrapper
      .get('[data-test="group-members-input"]')
      .setValue("bob carol");
    expect(submit.attributes("disabled")).toBeUndefined();
  });

  it("emits the split handles on submit and clears the form", async () => {
    const wrapper = mountForm();

    await wrapper.get('[data-test="group-title-input"]').setValue(" 九月小组 ");
    await wrapper
      .get('[data-test="group-members-input"]')
      .setValue("bob, carol、 dave");
    await wrapper.get("form").trigger("submit");

    expect(wrapper.emitted("create")?.[0]).toEqual([
      "九月小组",
      ["bob", "carol", "dave"],
    ]);
    expect(
      (
        wrapper.get('[data-test="group-title-input"]')
          .element as HTMLInputElement
      ).value,
    ).toBe("");
  });

  it("stays disabled while a create is in flight", () => {
    const wrapper = mount(CreateGroupForm, { props: { busy: true } });

    expect(
      wrapper.get('[data-test="create-group-submit"]').attributes("disabled"),
    ).toBeDefined();
  });
});
