import { describe, expect, it } from "vitest";
import {
  fieldMessages,
  isValidEmail,
  isValidUsername,
  validateEmail,
  validateLogin,
  validatePassword,
  validateRegistration,
  validateUsername,
} from "./validation";

describe("email validation", () => {
  it("accepts a well-formed address", () => {
    expect(validateEmail("alice@example.com")).toBeNull();
    // The form value is normalised before it is checked, so case and padding pass.
    expect(validateEmail("  Alice@Example.COM ")).toBeNull();
  });

  it("rejects empty, shapeless and label-less addresses", () => {
    expect(validateEmail("")?.code).toBe("REQUIRED");
    expect(validateEmail("   ")?.code).toBe("REQUIRED");
    expect(validateEmail("not-an-email")?.code).toBe("INVALID_FORMAT");
    expect(validateEmail("alice@example")?.code).toBe("INVALID_FORMAT");
    expect(validateEmail("alice@@example.com")?.code).toBe("INVALID_FORMAT");
    expect(validateEmail("alice@.com")?.code).toBe("INVALID_FORMAT");
    expect(validateEmail("alice@example.")?.code).toBe("INVALID_FORMAT");
    expect(validateEmail("alice@example..com")?.code).toBe("INVALID_FORMAT");
    expect(validateEmail("alice example@example.com")?.code).toBe(
      "INVALID_FORMAT",
    );
  });

  it("matches the backend's structural rule", () => {
    expect(isValidEmail("alice@example.com")).toBe(true);
    expect(isValidEmail("alice@example..com")).toBe(false);
    expect(isValidEmail("alice@.com")).toBe(false);
  });
});

describe("username validation", () => {
  it("accepts the documented alphabet and length", () => {
    expect(validateUsername("alice")).toBeNull();
    expect(validateUsername("alice_01")).toBeNull();
    expect(validateUsername("Alice")).toBeNull();
    expect(isValidUsername("alice_01")).toBe(true);
  });

  it("rejects empties, bad lengths and bad characters", () => {
    expect(validateUsername("")?.code).toBe("REQUIRED");
    expect(validateUsername("ab")?.code).toBe("TOO_SHORT");
    expect(validateUsername("a".repeat(33))?.code).toBe("TOO_LONG");
    expect(validateUsername("has space")?.code).toBe("INVALID_FORMAT");
    expect(validateUsername("has-dash")?.code).toBe("INVALID_FORMAT");
    expect(validateUsername("has.dot")?.code).toBe("INVALID_FORMAT");
  });
});

describe("password validation", () => {
  it("accepts a password with length, letters and digits", () => {
    expect(validatePassword("secret123")).toBeNull();
  });

  it("rejects empties, short, long and weak passwords", () => {
    expect(validatePassword("")?.code).toBe("REQUIRED");
    expect(validatePassword("short1")?.code).toBe("TOO_SHORT");
    expect(validatePassword(`${"a".repeat(128)}1`)?.code).toBe("TOO_LONG");
    expect(validatePassword("allletters")?.code).toBe("WEAK");
    expect(validatePassword("12345678")?.code).toBe("WEAK");
  });
});

describe("form validation", () => {
  it("reports every broken registration field at once", () => {
    const problems = validateRegistration({
      username: "A",
      email: "nope",
      password: "short",
      displayName: "",
    });

    expect(problems.map((entry) => entry.field)).toEqual([
      "username",
      "email",
      "password",
    ]);
  });

  it("accepts a complete registration form", () => {
    expect(
      validateRegistration({
        username: "alice",
        email: "alice@example.com",
        password: "secret123",
        displayName: "Alice",
      }),
    ).toEqual([]);
  });

  it("leaves login's password strength to the server but requires a value", () => {
    const problems = validateLogin({
      email: "alice@example.com",
      password: "",
    });

    expect(problems.map((entry) => entry.field)).toEqual(["password"]);
    expect(problems[0]?.code).toBe("REQUIRED");
  });

  it("rejects a malformed login email before any request is made", () => {
    const problems = validateLogin({ email: "nope", password: "secret123" });

    expect(problems.map((entry) => entry.code)).toEqual(["INVALID_FORMAT"]);
  });
});

describe("validation messages", () => {
  it("mirrors the backend's wording", () => {
    expect(validateEmail("nope")?.message).toBe("邮箱格式不正确");
    expect(validatePassword("allletters")?.message).toBe(
      "密码需同时包含字母和数字",
    );
    expect(validateUsername("ab")?.message).toBe("用户名至少 3 个字符");
  });

  it("collapses field errors into a lookup for rendering", () => {
    const messages = fieldMessages([
      { field: "email", code: "INVALID_FORMAT", message: "邮箱格式不正确" },
      { field: "password", code: "TOO_SHORT", message: "密码至少 8 个字符" },
    ]);

    expect(messages).toEqual({
      email: "邮箱格式不正确",
      password: "密码至少 8 个字符",
    });
  });
});
