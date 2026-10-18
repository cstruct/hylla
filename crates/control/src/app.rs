use leptos::prelude::*;
use leptos_meta::{MetaTags, Style, Title, provide_meta_context};
use leptos_router::{
    components::{A, Route, Router, Routes},
    hooks::{use_params_map, use_query_map},
    path,
};

use crate::{BackendInfo, CallInfo, Listing, MappingInfo};

const STYLE: &str = r#"
:root { color-scheme: light dark; --muted: #6b7280; --error: #b91c1c; --ok: #15803d; --line: #e5e7eb; }
@media (prefers-color-scheme: dark) { :root { --muted: #9ca3af; --error: #f87171; --ok: #4ade80; --line: #374151; } }
body { font: 15px/1.5 system-ui, sans-serif; margin: 0 auto; max-width: 64rem; padding: 1rem; }
nav { display: flex; gap: 1rem; border-bottom: 1px solid var(--line); padding-bottom: .5rem; margin-bottom: 1rem; }
nav strong { margin-right: auto; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: .35rem .5rem; border-bottom: 1px solid var(--line); vertical-align: top; }
td.num { text-align: right; font-variant-numeric: tabular-nums; }
.muted { color: var(--muted); }
.error { color: var(--error); }
.ok { color: var(--ok); }
form.inline { display: inline-flex; gap: .25rem; margin: 0; }
section { margin: 1rem 0; }
"#;

/// The page around the UI: its head, and the hydration scripts when the bundle is there.
pub fn shell(options: LeptosOptions, hydrate: bool) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                {hydrate.then(|| view! { <HydrationScripts options=options.clone() /> })}
                <MetaTags />
            </head>
            <body>
                <App />
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Title text="hylla" />
        <Style>{STYLE}</Style>
        <Router>
            <nav>
                <strong>"hylla"</strong>
                <A href="/">"Backends"</A>
                <A href="/mappings">"Mappings"</A>
            </nav>
            <main>
                <Routes fallback=|| view! { <p>"Not found."</p> }>
                    <Route path=path!("/") view=BackendsPage />
                    <Route path=path!("/mappings") view=MappingsPage />
                    <Route path=path!("/mappings/:server/:index") view=BrowsePage />
                </Routes>
            </main>
        </Router>
    }
}

#[server]
pub async fn get_backends() -> Result<Vec<BackendInfo>, ServerFnError> {
    Ok(expect_context::<std::sync::Arc<dyn crate::ControlPlane>>().backends())
}

#[server]
pub async fn get_mappings() -> Result<Vec<MappingInfo>, ServerFnError> {
    Ok(expect_context::<std::sync::Arc<dyn crate::ControlPlane>>().mappings())
}

#[server]
pub async fn list_objects(
    server: String,
    index: usize,
    prefix: String,
) -> Result<Listing, ServerFnError> {
    expect_context::<std::sync::Arc<dyn crate::ControlPlane>>()
        .list(&server, index, &prefix)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn delete_object(
    server: String,
    index: usize,
    path: String,
) -> Result<(), ServerFnError> {
    expect_context::<std::sync::Arc<dyn crate::ControlPlane>>()
        .delete(&server, index, &path)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn move_object(
    server: String,
    index: usize,
    from: String,
    to: String,
) -> Result<(), ServerFnError> {
    expect_context::<std::sync::Arc<dyn crate::ControlPlane>>()
        .rename(&server, index, &from, &to)
        .await
        .map_err(ServerFnError::new)
}

/// A time in milliseconds since the Unix epoch, as UTC.
fn time(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn call(call: Option<CallInfo>) -> AnyView {
    match call {
        None => view! { <span class="muted">"none yet"</span> }.into_any(),
        Some(call) => {
            let class = if call.outcome == "error" {
                "error"
            } else {
                "ok"
            };
            view! {
                <span class=class>{call.outcome.clone()}</span>
                " " {call.operation.clone()} " in " {call.duration_millis} " ms"
                <br />
                <span class="muted">{time(call.at_millis as i64)}</span>
                {call.error.map(|error| view! { <br /><span class="error">{error}</span> })}
            }
            .into_any()
        }
    }
}

#[component]
fn BackendsPage() -> impl IntoView {
    let backends = Resource::new(|| (), |_| get_backends());
    view! {
        <h1>"Backends"</h1>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || Suspend::new(async move {
                match backends.await {
                    Ok(backends) => view! {
                        <table>
                            <thead>
                                <tr><th>"Backend"</th><th>"Type"</th><th>"Last request"</th><th>"Last error"</th></tr>
                            </thead>
                            <tbody>
                                {backends.into_iter().map(|backend| view! {
                                    <tr>
                                        <td>{backend.name}</td>
                                        <td>{backend.kind}</td>
                                        <td>{call(backend.last)}</td>
                                        <td>{call(backend.last_error)}</td>
                                    </tr>
                                }).collect_view()}
                            </tbody>
                        </table>
                    }.into_any(),
                    Err(error) => view! { <p class="error">{error.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

#[component]
fn MappingsPage() -> impl IntoView {
    let mappings = Resource::new(|| (), |_| get_mappings());
    view! {
        <h1>"Mappings"</h1>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || Suspend::new(async move {
                match mappings.await {
                    Ok(mappings) => view! {
                        <table>
                            <thead>
                                <tr><th>"Server"</th><th>"Path prefix"</th><th>"Backend"</th><th>"Writable"</th></tr>
                            </thead>
                            <tbody>
                                {mappings.into_iter().map(|mapping| {
                                    let href = format!("/mappings/{}/{}", mapping.server, mapping.index);
                                    view! {
                                        <tr>
                                            <td>{mapping.server}</td>
                                            <td><a href=href>{mapping.path_prefix}</a></td>
                                            <td>{mapping.backend}</td>
                                            <td>{if mapping.writable { "yes" } else { "no" }}</td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    }.into_any(),
                    Err(error) => view! { <p class="error">{error.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

/// Percent-encodes a query parameter value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[component]
fn BrowsePage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let server = move || params.read().get("server").unwrap_or_default();
    let index = move || {
        params
            .read()
            .get("index")
            .and_then(|index| index.parse::<usize>().ok())
            .unwrap_or_default()
    };
    let prefix = move || query.read().get("prefix").unwrap_or_default();
    let delete = ServerAction::<DeleteObject>::new();
    let rename = ServerAction::<MoveObject>::new();
    let listing = Resource::new(
        move || {
            (
                server(),
                index(),
                prefix(),
                delete.version().get(),
                rename.version().get(),
            )
        },
        |(server, index, prefix, _, _)| list_objects(server, index, prefix),
    );
    let failure = move || {
        let delete = delete.value().get().and_then(Result::err);
        let rename = rename.value().get().and_then(Result::err);
        delete
            .or(rename)
            .map(|error| view! { <p class="error">{error.to_string()}</p> })
    };

    view! {
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || Suspend::new(async move {
                match listing.await {
                    Ok(listing) => {
                        let mapping = listing.mapping.clone();
                        let base = format!("/mappings/{}/{}", mapping.server, mapping.index);
                        let parent = listing.prefix.trim_end_matches('/').rsplit_once('/')
                            .map(|(parent, _)| format!("{parent}/"))
                            .unwrap_or_default();
                        let up = (!listing.prefix.is_empty())
                            .then(|| format!("{base}?prefix={}", encode(&parent)));
                        let upload = format!(
                            "/v1/mappings/{}/{}/upload?prefix={}&redirect=1",
                            encode(&mapping.server), mapping.index, encode(&listing.prefix)
                        );
                        view! {
                            <h1>{mapping.server.clone()} " " {mapping.path_prefix.clone()}</h1>
                            <p class="muted">
                                "Backend " {mapping.backend.clone()} ", "
                                {if mapping.writable { "writable" } else { "read only" }}
                                ". Listing " <code>{format!("{}/{}", mapping.path_prefix.trim_end_matches('/'), listing.prefix)}</code>
                            </p>
                            {up.map(|up| view! { <p><a href=up>"↑ Up"</a></p> })}
                            <table>
                                <thead>
                                    <tr><th>"Name"</th><th>"Size"</th><th>"Modified"</th>
                                        {mapping.writable.then(|| view! { <th>"Change"</th> })}
                                    </tr>
                                </thead>
                                <tbody>
                                    {listing.folders.iter().map(|folder| {
                                        let href = format!("{base}?prefix={}", encode(folder));
                                        let name = folder.strip_prefix(&listing.prefix).unwrap_or(folder).to_string();
                                        view! { <tr><td><a href=href>{name}</a></td><td></td><td></td>
                                            {mapping.writable.then(|| view! { <td></td> })}
                                        </tr> }
                                    }).collect_view()}
                                    {listing.objects.iter().map(|object| {
                                        let name = object.path.strip_prefix(&listing.prefix).unwrap_or(&object.path).to_string();
                                        let changes = mapping.writable.then(|| {
                                            let index = mapping.index;
                                            let (server, path) = (mapping.server.clone(), object.path.clone());
                                            let (move_server, from, to) = (server.clone(), path.clone(), path.clone());
                                            view! {
                                                <td>
                                                    <ActionForm action=rename attr:class="inline">
                                                        <input type="hidden" name="server" value=move_server />
                                                        <input type="hidden" name="index" value=index />
                                                        <input type="hidden" name="from" value=from />
                                                        <input type="text" name="to" value=to aria-label="New path" />
                                                        <button type="submit">"Move"</button>
                                                    </ActionForm>
                                                    <ActionForm action=delete attr:class="inline">
                                                        <input type="hidden" name="server" value=server />
                                                        <input type="hidden" name="index" value=index />
                                                        <input type="hidden" name="path" value=path />
                                                        <button type="submit">"Delete"</button>
                                                    </ActionForm>
                                                </td>
                                            }
                                        });
                                        view! {
                                            <tr>
                                                <td>{name}</td>
                                                <td class="num">{size(object.size)}</td>
                                                <td class="muted">{time(object.last_modified_millis)}</td>
                                                {changes}
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
                            {(listing.folders.is_empty() && listing.objects.is_empty())
                                .then(|| view! { <p class="muted">"Nothing here."</p> })}
                            {mapping.writable.then(|| view! {
                                <section>
                                    <h2>"Upload"</h2>
                                    <form method="post" enctype="multipart/form-data" action=upload>
                                        <input type="file" name="file" multiple=true required=true />
                                        <button type="submit">"Upload here"</button>
                                    </form>
                                </section>
                            })}
                        }.into_any()
                    }
                    Err(error) => view! { <p class="error">{error.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
        {failure}
    }
}

#[cfg(test)]
mod tests {
    use super::{size, time};

    #[test]
    fn times_and_sizes_read_well() {
        assert_eq!(time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(time(1_760_745_600_000), "2025-10-18 00:00:00 UTC");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(3 * 1024 * 1024 / 2), "1.5 MiB");
    }
}
