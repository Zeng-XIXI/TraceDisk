use std::sync::atomic::{AtomicBool, Ordering};

const LOCAL_ACCESS_CODE: &str = "123456";
const ACCESS_DENIED_EXIT_CODE: i32 = 10;

static ACCESS_GRANTED: AtomicBool = AtomicBool::new(false);

/// This is the single validation boundary. The local comparison is temporary;
/// a later version can replace this function with an HTTPS call to the Java
/// verification service without changing the frontend or protected commands.
async fn access_code_is_valid(code: &str) -> bool {
    constant_time_equal(code.as_bytes(), LOCAL_ACCESS_CODE.as_bytes())
}

#[tauri::command]
pub(crate) async fn verify_access_code(app: tauri::AppHandle, code: String) -> bool {
    let verified = access_code_is_valid(&code).await;
    ACCESS_GRANTED.store(verified, Ordering::Release);
    if !verified {
        app.exit(ACCESS_DENIED_EXIT_CODE);
    }
    verified
}

pub(crate) fn require_authenticated() -> Result<(), String> {
    if ACCESS_GRANTED.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("应用尚未通过访问验证码验证".into())
    }
}

fn constant_time_equal(candidate: &[u8], expected: &[u8]) -> bool {
    if candidate.len() != expected.len() {
        return false;
    }
    candidate
        .iter()
        .zip(expected)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::{constant_time_equal, LOCAL_ACCESS_CODE};

    #[test]
    fn accepts_only_the_current_local_access_code() {
        assert!(constant_time_equal(LOCAL_ACCESS_CODE.as_bytes(), b"123456"));
        assert!(!constant_time_equal(b"123455", b"123456"));
        assert!(!constant_time_equal(b"1234567", b"123456"));
        assert!(!constant_time_equal("１２３４５６".as_bytes(), b"123456"));
    }
}
