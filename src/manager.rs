use crate::{AxumSession, AxumSessionData, AxumSessionID, AxumSessionStore};
use axum::{
    http::{Request, StatusCode},
    response::IntoResponse,
};
use axum_extra::middleware::Next;
use chrono::{Duration, Utc};
use futures::executor::block_on;
use parking_lot::{Mutex, RwLockUpgradableReadGuard};
use std::collections::HashMap;
use tower_cookies::{Cookie, Cookies};
use uuid::Uuid;

///This manages the other services that can be seen in inner and gives access to the store.
/// the store is cloneable hence per each SqlxSession we clone it as we use thread Read write locks
/// to control any data that needs to be accessed across threads that cant be cloned.

pub async fn axum_session_runner<B>(
    mut req: Request<B>,
    next: Next<B>,
    store: AxumSessionStore,
) -> impl IntoResponse {
    // We Extract the Tower_Cookies Extensions Variable so we can add Cookies to it. Some reason can only be done here..?
    let cookies = match req.extensions().get::<Cookies>() {
        Some(cookies) => cookies,
        None => return Err(StatusCode::UNAUTHORIZED),
    };

    let session = AxumSession {
        id: {
            let store_ug = store.inner.upgradable_read();

            let id = if let Some(cookie) = cookies.get(&store.config.cookie_name) {
                (
                    AxumSessionID(Uuid::parse_str(cookie.value()).expect("`Could not parse Uuid")),
                    false,
                )
            } else {
                let new_id = loop {
                    let token = Uuid::new_v4();

                    if !store_ug.contains_key(&token.to_string()) {
                        break token;
                    }
                };

                (AxumSessionID(new_id), true)
            };

            if !id.1 {
                if let Some(m) = store_ug.get(&id.0.to_string()) {
                    let mut inner = m.lock();

                    if inner.expires < Utc::now() || inner.destroy {
                        // Database Session expired, reuse the ID but drop data.
                        inner.data = HashMap::new();
                    }

                    // Session is extended by making a request with valid ID
                    inner.expires = Utc::now() + store.config.lifespan;
                    inner.autoremove = Utc::now() + store.config.memory_lifespan;
                } else {
                    let mut store_wg = RwLockUpgradableReadGuard::upgrade(store_ug);

                    let mut sess = block_on(store.load_session(id.0.to_string()))
                        .ok()
                        .flatten()
                        .unwrap_or(AxumSessionData {
                            id: id.0 .0,
                            data: HashMap::new(),
                            expires: Utc::now() + Duration::hours(6),
                            destroy: false,
                            autoremove: Utc::now() + store.config.memory_lifespan,
                        });

                    if !sess.validate() || sess.destroy {
                        sess.data = HashMap::new();
                        sess.expires = Utc::now() + Duration::hours(6);
                        sess.autoremove = Utc::now() + store.config.memory_lifespan;
                    }

                    let mut cookie =
                        Cookie::new(store.config.cookie_name.clone(), id.0 .0.to_string());

                    cookie.make_permanent();

                    cookies.add(cookie);
                    store_wg.insert(id.0 .0.to_string(), Mutex::new(sess));
                }
            } else {
                // --- New ID was generated Lets make a session for it ---
                // Get exclusive write access to the map
                let mut store_wg = RwLockUpgradableReadGuard::upgrade(store_ug);

                // This branch runs less often, and we already have write access,
                // let's check if any sessions expired. We don't want to hog memory
                // forever by abandoned sessions (e.g. when a client lost their cookie)
                {
                    let timers = store.timers.upgradable_read();
                    // Throttle by memory lifespan - e.g. sweep every hour
                    if timers.last_expiry_sweep <= Utc::now() {
                        let mut timers = RwLockUpgradableReadGuard::upgrade(timers);
                        store_wg.retain(|_k, v| v.lock().autoremove > Utc::now());
                        timers.last_expiry_sweep = Utc::now() + store.config.memory_lifespan;
                    }
                }

                {
                    let timers = store.timers.upgradable_read();
                    // Throttle by database lifespan - e.g. sweep every 6 hours
                    if timers.last_database_expiry_sweep <= Utc::now() {
                        let mut timers = RwLockUpgradableReadGuard::upgrade(timers);
                        store_wg.retain(|_k, v| v.lock().autoremove > Utc::now());
                        block_on(store.cleanup()).unwrap();
                        timers.last_database_expiry_sweep = Utc::now() + store.config.lifespan;
                    }
                }

                let mut cookie = Cookie::new(store.config.cookie_name.clone(), id.0 .0.to_string());
                cookie.make_permanent();
                cookies.add(cookie);

                let sess = AxumSessionData {
                    id: id.0 .0,
                    data: HashMap::new(),
                    expires: Utc::now() + Duration::hours(6),
                    destroy: false,
                    autoremove: Utc::now() + store.config.memory_lifespan,
                };

                store_wg.insert(id.0 .0.to_string(), Mutex::new(sess));
            }

            id.0
        },
        store: store.clone(),
    };

    //Sets a clone of the Store in the Extensions for Direct usage and sets the Session for Direct usage
    req.extensions_mut().insert(store.clone());
    req.extensions_mut().insert(session.clone());

    let session_data = {
        session
            .store
            .inner
            .upgradable_read()
            .get(&session.id.0.to_string())
            .map(|sess| sess.lock().clone())
    };

    if let Some(data) = session_data {
        session.store.store_session(data).await.unwrap()
    }

    Ok(next.run(req).await)
}
