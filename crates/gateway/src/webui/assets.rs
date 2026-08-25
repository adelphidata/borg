use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;

/// The built `apps/web` SPA, embedded at compile time.
///
/// In debug builds rust-embed reads from disk, so `bun run dev` in `apps/web`
/// is picked up without recompiling borgd. Release builds bake the files in, so
/// one `borgd` binary is the daemon *and* its web UI.
///
/// `build.rs` creates the folder when it is missing, so a clean checkout
/// compiles before anyone has run the web build; an empty set falls back to
/// [`not_bundled`] rather than 404ing.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../apps/web/dist"]
struct WebAssets;

const INDEX: &str = "index.html";
/// Vite emits content-hashed build output here; everything else is a route.
const ASSET_DIR: &str = "assets/";

pub fn router() -> Router {
    Router::new()
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(|| async { serve(INDEX) }))
        .route(
            "/ui/{*path}",
            get(|uri: Uri| async move {
                let path = uri.path().trim_start_matches("/ui/");
                if WebAssets::get(path).is_some() {
                    return serve(path);
                }
                // A missing build artifact is a real 404. Answering it with
                // index.html would hand the browser HTML for a `<script>` and
                // surface as a MIME error instead of the actual problem.
                if path.starts_with(ASSET_DIR) {
                    return StatusCode::NOT_FOUND.into_response();
                }
                // Anything else is a client-side route; let the SPA resolve it.
                serve(INDEX)
            }),
        )
}

fn serve(path: &str) -> Response {
    let Some(file) = WebAssets::get(path) else {
        return not_bundled();
    };

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Vite content-hashes everything under assets/, so those are immutable.
    // index.html must revalidate or an upgraded binary keeps serving the old app.
    let cache_control = if path.starts_with(ASSET_DIR) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let etag = format!(
        "\"{:x}\"",
        u64::from_be_bytes(
            file.metadata.sha256_hash()[..8]
                .try_into()
                .unwrap_or_default()
        )
    );

    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache_control),
            (header::ETAG, etag.as_str()),
        ],
        file.data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// A file missing from the embed must fall back to the "not bundled"
    /// help page: a real HTML answer naming the fix, not an empty 200 the
    /// browser renders as a blank screen.
    #[tokio::test]
    async fn serve_missing_file_answers_not_bundled_help_page() {
        let resp = serve("__not_in_any_build__.bin");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .expect("fallback page must declare a content type"),
            "text/html; charset=utf-8"
        );
        let body = body_string(resp).await;
        assert!(
            body.contains("just web-build"),
            "fallback must name the fix command: {body}"
        );
    }

    /// A missing content-hashed build artifact is a real 404 — answering
    /// with index.html surfaces as a browser MIME error instead of the
    /// actual problem.
    #[tokio::test]
    async fn missing_hashed_asset_is_404_not_spa_fallback() {
        let resp = router()
            .oneshot(
                Request::builder()
                    .uri("/ui/assets/__no_such_chunk__.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

/// Shown when borgd was built without running the web build first.
fn not_bundled() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><meta charset=utf-8><title>Borg</title>\
         <h1>Borg</h1><p>The web UI is not bundled in this build. \
         Run <code>just web-build</code> and rebuild borgd.</p>",
    )
        .into_response()
}
