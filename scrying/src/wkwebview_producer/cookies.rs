//! `NSHTTPCookie` ↔ [`Cookie`] translation helpers used by the
//! producer's cookie-store API (`request_all_cookies` / `set_cookie`
//! / `delete_cookie`). Pure functions — no producer state.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{
    NSArray, NSDate, NSDictionary, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieExpires,
    NSHTTPCookieName, NSHTTPCookiePath, NSHTTPCookieSecure, NSHTTPCookieValue, NSString, NSURL,
};

use crate::Cookie;

/// Convert an `NSHTTPCookie` into our [`Cookie`] payload. All
/// translations happen on the main thread inside the cookie-store
/// completion block.
pub(super) fn cookie_from_ns(ns_cookie: &NSHTTPCookie) -> Cookie {
    let expires_at = ns_cookie.expiresDate().map(|d| d.timeIntervalSince1970());
    Cookie {
        name: ns_cookie.name().to_string(),
        value: ns_cookie.value().to_string(),
        domain: ns_cookie.domain().to_string(),
        path: ns_cookie.path().to_string(),
        expires_at,
        is_secure: ns_cookie.isSecure(),
        is_http_only: ns_cookie.isHTTPOnly(),
        same_site: None,
        partitioned: false,
    }
}

/// Build an `NSHTTPCookie` from our [`Cookie`] payload.
///
/// Default path: `NSHTTPCookie::cookieWithProperties:` with a
/// dictionary built from the documented property keys. Returns
/// `None` if the dictionary fails Apple's parser (typically:
/// missing / malformed name / value / domain / path).
///
/// HttpOnly path: when `cookie.is_http_only` is `true`, the
/// property-dictionary route can't represent that flag —
/// Apple's documented set of property keys (`NSHTTPCookieName`,
/// `NSHTTPCookieValue`, `NSHTTPCookieDomain`, `NSHTTPCookiePath`,
/// `NSHTTPCookieSecure`, `NSHTTPCookieExpires`,
/// `NSHTTPCookieMaximumAge`, etc.) does not include an
/// HttpOnly key, and `NSHTTPCookie` only sets `isHTTPOnly` when
/// it parses a `Set-Cookie` header that contains the
/// `HttpOnly` attribute. We synthesize that header line and
/// route through `cookiesWithResponseHeaderFields:forURL:` to
/// faithfully round-trip the flag.
///
/// Booleans go in as `"TRUE"` / `"FALSE"` `NSString`s — Apple's
/// docs explicitly support that, and it lets us avoid pulling
/// `objc2_foundation/NSNumber` (not in default features) just
/// for this. Expiry, when present, uses `NSDate` directly which
/// `NSHTTPCookie::cookieWithProperties:` accepts.
pub(super) fn ns_cookie_from(cookie: &Cookie) -> Option<Retained<NSHTTPCookie>> {
    if cookie.same_site.is_some() || cookie.partitioned {
        return None;
    }
    if cookie.is_http_only {
        return ns_cookie_from_http_only(cookie);
    }

    let name_ns = NSString::from_str(&cookie.name);
    let value_ns = NSString::from_str(&cookie.value);
    let domain_ns = NSString::from_str(&cookie.domain);
    let path_ns = NSString::from_str(&cookie.path);
    let secure_ns = NSString::from_str(if cookie.is_secure { "TRUE" } else { "FALSE" });

    let mut keys: Vec<&NSString> = vec![
        unsafe { NSHTTPCookieName },
        unsafe { NSHTTPCookieValue },
        unsafe { NSHTTPCookieDomain },
        unsafe { NSHTTPCookiePath },
        unsafe { NSHTTPCookieSecure },
    ];
    // Hold the strong refs alive so the `&AnyObject` slice we pass
    // to `NSDictionary::from_slices` stays valid until the dict is
    // built.
    let mut values: Vec<Retained<AnyObject>> = vec![
        unsafe { Retained::cast_unchecked(name_ns) },
        unsafe { Retained::cast_unchecked(value_ns) },
        unsafe { Retained::cast_unchecked(domain_ns) },
        unsafe { Retained::cast_unchecked(path_ns) },
        unsafe { Retained::cast_unchecked(secure_ns) },
    ];
    if let Some(unix_ts) = cookie.expires_at {
        let date = NSDate::dateWithTimeIntervalSince1970(unix_ts);
        keys.push(unsafe { NSHTTPCookieExpires });
        values.push(unsafe { Retained::cast_unchecked(date) });
    }

    let value_refs: Vec<&AnyObject> = values.iter().map(|v| &**v).collect();
    let dict = NSDictionary::from_slices(&keys, &value_refs);
    unsafe { NSHTTPCookie::cookieWithProperties(&dict) }
}

/// Build an `NSHTTPCookie` with `HttpOnly = true` by feeding a
/// synthesized `Set-Cookie` response-header line through
/// `NSHTTPCookie::cookiesWithResponseHeaderFields:forURL:`.
///
/// Set-Cookie syntax (RFC 6265): `name=value;
/// Domain=<d>; Path=<p>; Max-Age=<n>; Secure; HttpOnly`. Use
/// `Max-Age` rather than `Expires` so we don't have to format
/// the date ourselves (Apple's parser handles both, and
/// Max-Age is computed in seconds-from-now from the Unix
/// timestamp).
fn ns_cookie_from_http_only(cookie: &Cookie) -> Option<Retained<NSHTTPCookie>> {
    let mut header = format!("{}={}", cookie.name, cookie.value);
    if !cookie.domain.is_empty() {
        header.push_str(&format!("; Domain={}", cookie.domain));
    }
    if !cookie.path.is_empty() {
        header.push_str(&format!("; Path={}", cookie.path));
    }
    if let Some(unix_ts) = cookie.expires_at {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let max_age = (unix_ts - now).max(0.0) as u64;
        header.push_str(&format!("; Max-Age={max_age}"));
    }
    if cookie.is_secure {
        header.push_str("; Secure");
    }
    header.push_str("; HttpOnly");

    // Build the {"Set-Cookie": "<header>"} dictionary the parser
    // expects. The accompanying URL just needs a scheme + host
    // matching the cookie's domain so Apple's parser accepts
    // the cookie's domain attribute against it.
    let key = NSString::from_str("Set-Cookie");
    let value = NSString::from_str(&header);
    let header_dict = NSDictionary::from_slices(&[&*key], &[&*value]);

    let scheme = if cookie.is_secure { "https" } else { "http" };
    let host = if cookie.domain.is_empty() {
        "localhost".to_string()
    } else {
        cookie.domain.trim_start_matches('.').to_string()
    };
    let url_str = NSString::from_str(&format!("{scheme}://{host}/"));
    let url = NSURL::URLWithString(&url_str)?;

    let cookies: Retained<NSArray<NSHTTPCookie>> =
        NSHTTPCookie::cookiesWithResponseHeaderFields_forURL(&header_dict, &url);
    if cookies.count() == 0 {
        return None;
    }
    Some(cookies.objectAtIndex(0))
}
