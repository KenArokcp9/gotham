use crate::state_data::AuthorizationToken;
use futures_util::future::{self, FutureExt, TryFutureExt};
use gotham::anyhow;
use gotham::handler::HandlerFuture;
use gotham::helpers::http::response::create_empty_response;
use gotham::hyper::header::{HeaderMap, AUTHORIZATION};
use gotham::hyper::StatusCode;
use gotham::middleware::{Middleware, NewMiddleware};
use gotham::state::{request_id, FromState, State};
use jsonwebtoken::{decode, DecodingKey, Validation};
use log::trace;
use serde::Deserialize;
use std::marker::PhantomData;
use std::panic::RefUnwindSafe;
use std::pin::Pin;

const DEFAULT_SCHEME: &str = "Bearer";

/// This middleware verifies that JSON Web Token
/// credentials, provided via the HTTP `Authorization`
/// header, are extracted, parsed, and validated
/// according to best practices before passing control
/// to middleware beneath this middleware for a given
/// mount point.
///
/// Requests that lack the `Authorization` header are
/// returned with the Status Code `400: Bad Request`.
/// Tokens that fail validation cause the middleware
/// to return Status Code `401: Unauthorized`.
///
/// Example:
/// ```rust
/// use futures_util::future::{self, FutureExt};
/// use gotham::handler::HandlerFuture;
/// use gotham::helpers::http::response::create_empty_response;
/// use gotham::hyper::{Response, StatusCode};
/// use gotham::pipeline::{finalize_pipeline_set, new_pipeline, new_pipeline_set};
/// use gotham::router::builder::*;
/// use gotham::router::Router;
/// use gotham::state::{FromState, State};
/// use gotham_middleware_jwt::{AuthorizationToken, JwtMiddleware};
/// use serde::Deserialize;
/// use std::pin::Pin;
///
/// #[derive(Deserialize, Debug)]
/// struct Claims {
///     sub: String,
///     exp: usize,
/// }
///
/// fn handler(state: State) -> Pin<Box<HandlerFuture>> {
///     {
///         let token = AuthorizationToken::<Claims>::borrow_from(&state);
///         // token -> TokenData
///     }
///     let res = create_empty_response(&state, StatusCode::OK);
///     future::ok((state, res)).boxed()
/// }
///
/// fn router() -> Router {
///     let pipelines = new_pipeline_set();
///     let (pipelines, defaults) = pipelines.add(
///         new_pipeline()
///             .add(JwtMiddleware::<Claims>::new("secret"))
///             .build(),
///     );
///     let default_chain = (defaults, ());
///     let pipeline_set = finalize_pipeline_set(pipelines);
///     build_router(default_chain, pipeline_set, |route| {
///         route.get("/").to(handler);
///     })
/// }
///
/// # fn main() {
/// #    let _ = router();
/// # }
/// ```
pub struct JwtMiddleware<T> {
    secret: String,
    validation: Validation,
    scheme: String,
    claims: PhantomData<T>,
}

impl<T> JwtMiddleware<T>
where
    T: for<'de> Deserialize<'de> + Send + Sync,
{
    /// Creates a JWTMiddleware instance from the provided secret,
    /// which, by default, uses HS256 as the crypto scheme.
    pub fn new<S: Into<String>>(secret: S) -> Self {
        let validation = Validation::default();

        Self {
            secret: secret.into(),
            validation,
            scheme: DEFAULT_SCHEME.into(),
            claims: PhantomData,
        }
    }

    /// Create a new instance of the middleware by appending new
    /// validation constraints.
    pub fn validation(self, validation: Validation) -> Self {
        Self { validation, ..self }
    }

    /// Create a new instance of the middleware with a custom scheme
    pub fn scheme<S: Into<String>>(self, scheme: S) -> Self {
        Self {
            scheme: scheme.into(),
            ..self
        }
    }
}

impl<T> Middleware for JwtMiddleware<T>
where
    T: for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    fn call<Chain>(self, mut state: State, chain: Chain) -> Pin<Box<HandlerFuture>>
    where
        Chain: FnOnce(State) -> Pin<Box<HandlerFuture>> + 'static,
        Self: Sized,
    {
        trace!("[{}] pre-chain jwt middleware", request_id(&state));

        let token = match HeaderMap::borrow_from(&state).get(AUTHORIZATION) {
            Some(h) => match h.to_str() {
                Ok(hx) => hx.get((self.scheme.len() + 1)..),
                _ => None,
            },
            _ => None,
        };

        if token.is_none() {
            trace!("[{}] bad request jwt middleware", request_id(&state));
            let res = create_empty_response(&state, StatusCode::BAD_REQUEST);
            return future::ok((state, res)).boxed();
        }

        let decoding_key = DecodingKey::from_secret(self.secret.as_ref());
        match decode::<T>(token.unwrap(), &decoding_key, &self.validation) {
            Ok(token) => {
                state.put(AuthorizationToken(token));

                let res = chain(state).and_then(|(state, res)| {
                    trace!("[{}] post-chain jwt middleware", request_id(&state));
                    future::ok((state, res))
                });

                res.boxed()
            }
            Err(e) => {
                trace!("[{}] error jwt middleware", e);
                let res = create_empty_response(&state, StatusCode::UNAUTHORIZED);
                future::ok((state, res)).boxed()
            }
        }
    }
}

impl<T> NewMiddleware for JwtMiddleware<T>
where
    T: for<'de> Deserialize<'de> + RefUnwindSafe + Send + Sync + 'static,
{
    type Instance = Self;

    fn new_middleware(&self) -> anyhow::Result<Self::Instance> {
        Ok(Self {
            secret: self.secret.clone(),
            validation: self.validation.clone(),
            scheme: self.scheme.clone(),
            claims: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gotham::handler::HandlerFuture;
    use gotham::pipeline::{new_pipeline, single_pipeline};
    use gotham::router::builder::*;
    use gotham::router::Router;
    use gotham::state::State;
    use gotham::test::TestServer;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::Serialize;

    const SECRET: &str = "some-secret";

    #[derive(Debug, Deserialize, Serialize)]
    pub struct Claims {
        sub: String,
        exp: usize,
    }

    #[cfg_attr(feature = "cargo-clippy", allow(clippy::match_wild_err_arm))]
    fn token(alg: Algorithm) -> String {
        let claims = &Claims {
            sub: "test@example.net".to_owned(),
            exp: 10_000_000_000,
        };

        let header = Header {
            kid: Some("signing-key".to_owned()),
            alg,
            ..Default::default()
        };

        match encode(&header, &claims, &EncodingKey::from_secret(SECRET.as_ref())) {
            Ok(t) => t,
            Err(_) => panic!(),
        }
    }

    fn handler(state: State) -> Pin<Box<HandlerFuture>> {
        {
            // If this compiles, the token is available.
            let _ = AuthorizationToken::<Claims>::borrow_from(&state);
        }
        let res = create_empty_response(&state, StatusCode::OK);
        future::ok((state, res)).boxed()
    }

    fn default_jwt_middleware() -> JwtMiddleware<Claims> {
        JwtMiddleware::<Claims>::new(SECRET).validation(Validation::default())
    }

    fn jwt_middleware_with_scheme(scheme: &str) -> JwtMiddleware<Claims> {
        JwtMiddleware::<Claims>::new(SECRET)
            .validation(Validation::default())
            .scheme(scheme)
    }

    fn router(middleware: JwtMiddleware<Claims>) -> Router {
        // Create JWTMiddleware with HS256 algorithm (default).

        let (chain, pipelines) = single_pipeline(new_pipeline().add(middleware).build());

        build_router(chain, pipelines, |route| {
            route.get("/").to(handler);
        })
    }

    #[test]
    fn jwt_middleware_no_header_test() {
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        let res = test_server
            .client()
            .get("https://example.com")
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn jwt_middleware_no_value_test() {
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, "".parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn jwt_middleware_no_token_test() {
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, "Bearer ".parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn jwt_middleware_malformatted_token_test() {
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, "Bearer xxxx".parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn jwt_middleware_malformatted_token_no_space_test() {
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, "Bearer".parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn jwt_middleware_invalid_algorithm_token_test() {
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, "Bearer eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJleHAiOjE1MzA0MDE1MjcsImlhdCI6MTUzMDM5OTcyN30.lhg7K9SK3DXsvimVb6o_h6VcsINtkT-qHR-tvDH1bGI".parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn jwt_middleware_valid_token_test() {
        let token = token(Algorithm::HS256);
        let test_server = TestServer::new(router(default_jwt_middleware())).unwrap();
        println!("Requesting with token... {}", token);
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, format!("Bearer {}", token).parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn jwt_middleware_valid_token_custom_scheme() {
        let token = token(Algorithm::HS256);
        let middleware = jwt_middleware_with_scheme("Token");
        let test_server = TestServer::new(router(middleware)).unwrap();
        println!("Requesting with token... {}", token);
        let res = test_server
            .client()
            .get("https://example.com")
            .with_header(AUTHORIZATION, format!("Token {}", token).parse().unwrap())
            .perform()
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }
}
