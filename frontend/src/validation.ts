/**
 * Client-side mirror of the backend's validation rules.
 *
 * The backend is authoritative; this exists so the user sees a problem before a
 * round trip. The rules and the message strings are kept identical to
 * `backend/crates/auth/src/validation.rs` and its Chinese messages, so a local
 * rejection and a server rejection look the same to the user.
 *
 * The error *shape* is not duplicated: `FieldError` and `FieldErrorCode` come
 * from the generated contract (`src/generated/`).
 */

import type { FieldError } from "./generated/FieldError";
import type { FieldErrorCode } from "./generated/FieldErrorCode";

/** Shortest accepted `@handle`, mirroring the backend and the database check. */
export const USERNAME_MIN_CHARS = 3;
/** Longest accepted `@handle`. */
export const USERNAME_MAX_CHARS = 32;
/** Shortest accepted password. */
export const PASSWORD_MIN_CHARS = 8;
/** Longest accepted password. */
export const PASSWORD_MAX_CHARS = 128;
/** Longest accepted email. */
export const EMAIL_MAX_CHARS = 254;
/** Longest accepted display name. */
export const DISPLAY_NAME_MAX_CHARS = 64;

const USERNAME_PATTERN = /^[a-z0-9_]{3,32}$/;

/** The registration form as the view holds it (display name may be blank). */
export interface RegisterForm {
  username: string;
  email: string;
  password: string;
  displayName: string;
}

/** The login form as the view holds it. */
export interface LoginForm {
  email: string;
  password: string;
}

/** Trim and lowercase an email into its stored form. */
export function normalizeEmail(value: string): string {
  return value.trim().toLowerCase();
}

/** Trim and lowercase a username into its stored form. */
export function normalizeUsername(value: string): string {
  return value.trim().toLowerCase();
}

/**
 * Count Unicode scalar values, matching Rust's `chars().count()`.
 *
 * `String.prototype.length` counts UTF-16 code units, so it would disagree with
 * the backend for astral characters (emoji, some CJK extensions).
 */
export function charCount(value: string): number {
  return [...value].length;
}

/**
 * Whether a *normalised* email matches the accepted shape — the same structural
 * rule the backend applies: exactly one `@`, both sides non-empty, no
 * whitespace, and a dotted domain with no empty label.
 */
export function isValidEmail(email: string): boolean {
  if (email.length === 0 || charCount(email) > EMAIL_MAX_CHARS) return false;
  if (/\s/.test(email)) return false;

  const parts = email.split("@");
  if (parts.length !== 2) return false;

  const local = parts[0];
  const domain = parts[1];
  if (local === undefined || domain === undefined) return false;
  if (local.length === 0 || domain.length === 0) return false;
  if (!domain.includes(".")) return false;
  if (domain.split(".").some((label) => label.length === 0)) return false;

  return true;
}

/** Whether a *normalised* username matches `^[a-z0-9_]{3,32}$`. */
export function isValidUsername(username: string): boolean {
  return USERNAME_PATTERN.test(username);
}

/** Whether a password satisfies the length + letter/digit policy. */
export function isStrongPassword(password: string): boolean {
  const length = charCount(password);
  if (length < PASSWORD_MIN_CHARS || length > PASSWORD_MAX_CHARS) return false;
  return /[A-Za-z]/.test(password) && /[0-9]/.test(password);
}

/** Validate a username, returning the field problem or `null`. */
export function validateUsername(raw: string): FieldError | null {
  const username = normalizeUsername(raw);
  if (username.length === 0) {
    return problem("username", "REQUIRED", "请输入用户名");
  }
  const length = charCount(username);
  if (length < USERNAME_MIN_CHARS) {
    return problem("username", "TOO_SHORT", "用户名至少 3 个字符");
  }
  if (length > USERNAME_MAX_CHARS) {
    return problem("username", "TOO_LONG", "用户名最多 32 个字符");
  }
  if (!isValidUsername(username)) {
    return problem(
      "username",
      "INVALID_FORMAT",
      "用户名只能包含小写字母、数字和下划线",
    );
  }
  return null;
}

/** Validate an email (normalising it first), returning the problem or `null`. */
export function validateEmail(raw: string): FieldError | null {
  const email = normalizeEmail(raw);
  if (email.length === 0) {
    return problem("email", "REQUIRED", "请输入邮箱");
  }
  if (!isValidEmail(email)) {
    return problem("email", "INVALID_FORMAT", "邮箱格式不正确");
  }
  return null;
}

/** Validate a password, returning the problem or `null`. */
export function validatePassword(password: string): FieldError | null {
  if (password.length === 0) {
    return problem("password", "REQUIRED", "请输入密码");
  }
  const length = charCount(password);
  if (length < PASSWORD_MIN_CHARS) {
    return problem("password", "TOO_SHORT", "密码至少 8 个字符");
  }
  if (length > PASSWORD_MAX_CHARS) {
    return problem("password", "TOO_LONG", "密码最多 128 个字符");
  }
  if (!isStrongPassword(password)) {
    return problem("password", "WEAK", "密码需同时包含字母和数字");
  }
  return null;
}

/** Validate an optional display name; blank means "use the username". */
export function validateDisplayName(raw: string): FieldError | null {
  const trimmed = raw.trim();
  if (trimmed.length === 0) return null;
  if (charCount(trimmed) > DISPLAY_NAME_MAX_CHARS) {
    return problem("display_name", "TOO_LONG", "昵称最多 64 个字符");
  }
  return null;
}

/** Validate every registration field in one pass. */
export function validateRegistration(form: RegisterForm): FieldError[] {
  const problems = [
    validateUsername(form.username),
    validateEmail(form.email),
    validatePassword(form.password),
    validateDisplayName(form.displayName),
  ];
  return problems.filter((entry): entry is FieldError => entry !== null);
}

/** Validate the login form. */
export function validateLogin(form: LoginForm): FieldError[] {
  const problems: FieldError[] = [];
  const email = validateEmail(form.email);
  if (email !== null) problems.push(email);
  if (form.password.length === 0) {
    problems.push(problem("password", "REQUIRED", "请输入密码"));
  }
  return problems;
}

/** Collapse field problems into a `field -> message` map for rendering. */
export function fieldMessages(
  fields: readonly FieldError[],
): Record<string, string> {
  const messages: Record<string, string> = {};
  for (const entry of fields) {
    messages[entry.field] = entry.message;
  }
  return messages;
}

function problem(
  field: string,
  code: FieldErrorCode,
  message: string,
): FieldError {
  return { field, code, message };
}
