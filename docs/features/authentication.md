# Authentication & 2FA

icloudpd-rs authenticates with Apple's iCloud services using the same protocol as icloud.com.

## SRP-6a

Authentication uses Apple's custom SRP-6a (Secure Remote Password) implementation. Your password is never sent to Apple's servers — instead, a zero-knowledge proof is exchanged.

The flow:

1. Client sends username to Apple's auth endpoint
2. Apple returns a salt and server public value
3. Client computes a proof using the password, salt, and server value
4. Apple verifies the proof and returns a session token
5. Both sides derive a shared key without the password crossing the wire

## Two-Factor Authentication

After SRP authentication, Apple may require a 2FA code. icloudpd-rs supports trusted device codes — a 6-digit code is pushed to your other Apple devices, and you enter it at the prompt.

Once verified, the session can be trusted so that future runs don't require 2FA again (subject to Apple's trust expiry).

> [!NOTE]
> SMS-based 2FA is not yet supported. See the project roadmap for planned features.

## Session Persistence

After successful authentication, session cookies and trust tokens are saved to the [`--cookie-directory`](../cli/cookie-directory.md) (default `~/.icloudpd-rs`). On subsequent runs, these are loaded to avoid re-authentication.

Files are written with `0o600` permissions. Corrupt files are detected and recovered automatically.

### Current Limitations

- Trust token expiry is not tracked — sessions may fail silently after expiry
- No proactive session refresh during long syncs
- No lock file to prevent concurrent instances from corrupting state
- Session is not accessible from the download layer, so mid-sync re-auth is not possible

## Related Flags

- [`--username`](../cli/username.md)
- [`--password`](../cli/password.md)
- [`--auth-only`](../cli/auth-only.md)
- [`--domain`](../cli/domain.md)
- [`--cookie-directory`](../cli/cookie-directory.md)
