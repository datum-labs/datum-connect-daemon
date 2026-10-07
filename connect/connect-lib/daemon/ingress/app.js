// The Datum Connect add-on's page in Home Assistant.
//
// One page, two servers: while the add-on has no key, `datum-connect-daemon
// setup` serves it and runs pairing; once paired, the daemon serves it and
// shows the tunnel. When one hands over to the other, this page notices the
// mode change and reloads, which also picks up the new server's CSRF token.
//
// Every URL is relative: the page runs under Home Assistant's ingress path
// (/api/hassio_ingress/<token>/). Everything shown is built with
// textContent, never innerHTML, because display names and messages come
// from elsewhere.
"use strict";

(function () {
  var csrf = (document.querySelector('meta[name="datum-csrf"]') || {}).content || "";
  var loadedMode = document.body.getAttribute("data-mode");
  var root = document.getElementById("app");
  var NEW_ADDRESS_NOTE =
    "A new address can take 10–20 minutes before it works in every browser.";

  // What the person picked in the project list, kept across re-renders.
  var picked = null;
  // The last state seen, to explain a server that went away.
  var last = null;
  var busy = false;
  var actionResult = null;
  var timer = null;
  // What is on screen, so that an unchanged state is not drawn again every
  // second, which would close a project list someone has open.
  var rendered = null;
  // When the code on screen expires, in this browser's clock, and which
  // code that is. The server sends seconds left, not a time, so the two
  // clocks need not agree.
  var codeDeadline = null;
  var codeFor = null;

  function el(tag, attrs) {
    var node = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (k) {
        var v = attrs[k];
        if (v === null || v === undefined || v === false) return;
        if (k === "text") node.textContent = v;
        else if (k === "onclick") node.addEventListener("click", v);
        else if (k === "onchange") node.addEventListener("change", v);
        else node.setAttribute(k, v === true ? "" : v);
      });
    }
    for (var i = 2; i < arguments.length; i++) {
      var c = arguments[i];
      if (c === null || c === undefined || c === false) continue;
      node.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    }
    return node;
  }

  function show() {
    root.replaceChildren.apply(root, [el("h1", { text: "Datum Connect" })].concat(
      Array.prototype.slice.call(arguments).filter(Boolean)));
  }

  function card() {
    return el.apply(null, ["section", { class: "card" }].concat(Array.prototype.slice.call(arguments)));
  }

  function schedule(ms) {
    clearTimeout(timer);
    timer = setTimeout(poll, ms);
  }

  function poll() {
    fetch("api/state", { cache: "no-store", credentials: "same-origin" })
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.json();
      })
      .then(function (state) {
        if (state.mode !== loadedMode) {
          location.reload();
          return;
        }
        last = state;
        syncDeadline(state);
        if (renderKey(state) !== rendered) render(state);
        schedule(state.mode === "paired" ? 10000 : 1000);
      })
      .catch(function () {
        renderAway();
        schedule(2000);
      });
  }

  function post(path, body) {
    busy = true;
    if (last) render(last);
    return fetch(path, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", "X-Datum-Connect-CSRF": csrf },
      body: JSON.stringify(body || {}),
    })
      .then(function (r) {
        return r.json().catch(function () { return {}; }).then(function (j) {
          if (r.status === 403 && j.error === "csrf") {
            // A different server answered than the one that served this
            // page: start over with its token.
            location.reload();
            return null;
          }
          if (!r.ok) throw new Error(j.message || "Something went wrong (HTTP " + r.status + ").");
          return j;
        });
      })
      .finally(function () {
        busy = false;
      });
  }

  // ---- Setup (not paired yet) ----

  // The seconds left change on every poll; the countdown ticks by itself,
  // so they do not count as a change worth drawing again.
  function renderKey(state) {
    return (busy ? "1" : "0") + JSON.stringify(state, function (k, v) {
      return k === "remaining_secs" ? undefined : v;
    });
  }

  function syncDeadline(state) {
    var c = state && state.mode === "setup" && state.setup && state.setup.code;
    if (!c) {
      codeDeadline = null;
      codeFor = null;
      return;
    }
    var d = Date.now() + c.remaining_secs * 1000;
    // A new code (asked for, or renewed on expiry) starts over; the same
    // one is only corrected if it drifted, so the countdown does not jitter.
    if (codeFor !== c.user_code || codeDeadline === null || Math.abs(d - codeDeadline) > 1500) {
      codeDeadline = d;
      codeFor = c.user_code;
    }
  }

  function countdownText() {
    if (codeDeadline === null) return "";
    var left = Math.max(0, Math.ceil((codeDeadline - Date.now()) / 1000));
    if (left === 0) return "This code has expired; a new one appears here in a moment.";
    var m = Math.floor(left / 60);
    var sec = left % 60;
    return "Expires in " + m + ":" + (sec < 10 ? "0" : "") + sec;
  }

  function tickCountdown() {
    var node = document.getElementById("countdown");
    if (node) node.textContent = countdownText();
  }

  function render(state) {
    rendered = renderKey(state);
    if (state.mode === "paired") renderPaired(state.paired);
    else renderSetup(state.setup);
  }

  function errorLine(e) {
    return el("p", { class: "error", text: e && e.message ? e.message : String(e) });
  }

  function act(path, body) {
    post(path, body).then(function (s) {
      if (s) { last = s; syncDeadline(s); render(s); }
      schedule(300);
    }).catch(function (e) {
      if (last) render(last);
      root.appendChild(errorLine(e));
    });
  }

  function connectButton(label) {
    return el("button", { onclick: function () { act("api/connect"); }, disabled: busy }, label);
  }

  function renderSetup(s) {
    switch (s.phase) {
      case "idle":
        return show(card(
          el("h2", { text: "Connect this Home Assistant to Datum" }),
          el("p", { text: "Datum gives this Home Assistant a public HTTPS address, with no ports opened on your router. Sign in to Datum to connect it; nothing is created until you choose a project." }),
          connectButton("Connect to Datum")));
      case "starting":
        return show(card(el("p", { class: "muted", text: "Asking Datum for a code…" })));
      case "code":
        return show(card(
          el("h2", { text: "Approve this Home Assistant" }),
          s.code_renewed ? el("p", { class: "warn", text: "The last code expired before it was approved. Here is a new one." }) : null,
          el("p", null, "1. Open the Datum approval page and sign in:"),
          el("p", null, el("a", { class: "button", href: s.code.url, target: "_blank", rel: "noopener noreferrer" }, "Open the approval page")),
          el("p", null, "2. Check that it shows this code, then approve:"),
          el("div", { class: "code", text: s.code.user_code }),
          el("div", { class: "code-row" },
            el("span", { id: "countdown", class: "countdown", role: "timer", text: countdownText() }),
            el("button", {
              class: "secondary",
              disabled: busy,
              onclick: function () { act("api/new-code"); },
            }, "Get a new code")),
          el("p", { class: "note small", text: "If Datum's page says “Something went wrong”, click Get a new code and approve again right away." }),
          el("p", { class: "muted small", text: "A new code also appears here by itself when this one expires. The approval page says “datumctl”: that is expected." })));
      case "approved":
        return show(card(
          el("p", { class: "ok", text: "Signed in as " + (s.email || "you") + "." }),
          el("p", { class: "muted", text: "Finding your projects…" })));
      case "choose":
        return renderChoose(s);
      case "working":
        return show(card(
          s.email ? el("p", { class: "ok", text: "Signed in as " + s.email + "." }) : null,
          el("h2", { text: "Connecting to " + (s.project || "your project") }),
          steps(s)));
      case "done":
        return show(card(
          el("h2", { class: "ok", text: "Done. Starting the tunnel…" }),
          steps(s),
          el("p", { class: "muted", text: "This page switches to the tunnel's status by itself once it is up." }),
          el("p", { class: "note", text: NEW_ADDRESS_NOTE })));
      case "failed":
        return renderFailed(s);
      default:
        return show(card(el("p", { text: "Unexpected state: " + s.phase })));
    }
  }

  function steps(s) {
    var list = [
      ["Creating a service account", s.steps.service_account],
      ["Granting it access to " + (s.project || "the project"), s.steps.access],
      ["Saving the key", s.steps.key],
    ];
    var current = list.findIndex(function (x) { return !x[1]; });
    return el.apply(null, ["ol", { class: "steps" }].concat(list.map(function (x, i) {
      return el("li", { class: x[1] ? "done" : (i === current && s.phase === "working" ? "now" : null), text: x[0] });
    })));
  }

  function renderChoose(s) {
    var projects = s.projects || [];
    if (picked === null || !projects.some(function (p) { return p.id === picked; })) {
      var suggested = projects.filter(function (p) { return p.id === s.suggested; })[0];
      picked = suggested ? suggested.id : (projects.length === 1 ? projects[0].id : "");
    }
    var select = el("select", {
      id: "project",
      onchange: function (e) { picked = e.target.value; render(last); },
    });
    if (projects.length > 1) select.appendChild(el("option", { value: "", text: "Choose a project…" }));
    var orgs = [];
    projects.forEach(function (p) { if (orgs.indexOf(p.organization) < 0) orgs.push(p.organization); });
    orgs.forEach(function (org) {
      var group = el("optgroup", { label: "Organization " + org });
      projects.filter(function (p) { return p.organization === org; }).forEach(function (p) {
        group.appendChild(el("option", {
          value: p.id,
          text: p.display_name === p.id ? p.id : p.display_name + " (" + p.id + ")",
        }));
      });
      select.appendChild(group);
    });
    select.value = picked;
    return show(card(
      el("p", { class: "ok", text: "Signed in as " + (s.email || "you") + "." }),
      el("h2", null, el("label", { for: "project", text: projects.length === 1 ? "Confirm the project" : "Choose a project" })),
      el("p", { class: "muted small", text: "The tunnel, and the service account this Home Assistant uses, are created in this project." }),
      select,
      el("button", {
        disabled: busy || !picked,
        onclick: function () { act("api/project", { project: picked }); },
      }, "Continue"),
      el("p", { class: "muted small", text: "You can also set project on the add-on's Configuration tab and click Save." })));
  }

  function renderFailed(s) {
    var e = s.error || { kind: "failed", message: "Something went wrong." };
    var title = {
      expired: "The code expired",
      denied: "Access was denied",
      forbidden: "Almost there: access could not be granted",
    }[e.kind] || "Connecting did not work";
    return show(card(
      el("h2", { class: "error", text: title }),
      el("p", { text: e.message }),
      connectButton(e.kind === "expired" ? "Get a new code" : "Try again")));
  }

  // ---- Paired (the daemon is running) ----

  function renderPaired(p) {
    var parts = [];
    if (actionResult) {
      parts.push(card(
        el("h2", { text: actionResult.title }),
        el("p", { text: actionResult.text }),
        actionResult.account ? el("p", { class: "small", text: "The service account " + actionResult.account + " still exists in Datum. Delete it in the Datum portal, under the project's Service accounts, if you no longer need it." }) : null));
      return show.apply(null, parts);
    }
    var tunnels = p.tunnels || [];
    if (p.error) parts.push(card(el("p", { class: "warn", text: p.error })));
    if (!tunnels.length && !p.error) parts.push(card(el("p", { class: "muted", text: "No tunnel yet. The add-on creates it as it starts; this page updates by itself." })));
    tunnels.forEach(function (t) {
      parts.push(card(
        el("h2", null, t.label + " ", el("span", { class: "badge " + stateClass(t.state), text: stateText(t.state) })),
        t.address
          ? el("p", null, el("a", { class: "button", href: t.address, target: "_blank", rel: "noopener noreferrer" }, t.address))
          : el("p", { class: "muted", text: "The public address appears here once it is assigned." }),
        el("p", { class: "note small", text: NEW_ADDRESS_NOTE }),
        el("dl", { class: "facts" },
          el("dt", { text: "Edge protection" }), el("dd", { text: wafText(t.edge) }),
          el("dt", { text: "Request timeout" }), el("dd", { text: timeoutText(t.edge) }),
          t.portal_url ? el("dt", { text: "Portal" }) : null,
          t.portal_url ? el("dd", null, el("a", { href: t.portal_url, target: "_blank", rel: "noopener noreferrer", text: "Open in the Datum portal" })) : null)));
    });
    parts.push(card(
      el("dl", { class: "facts" },
        el("dt", { text: "Project" }), el("dd", { text: p.project }),
        el("dt", { text: "Service account" }), el("dd", { text: p.service_account || "unknown" }),
        el("dt", { text: "Key" }), el("dd", { text: p.key_source === "paired" ? "Made by connecting to Datum" : "Provided on the Configuration tab" }))));
    parts.push(card(
      el("h2", { text: "Connection" }),
      p.can_forget
        ? el("p", { class: "muted small", text: "Re-pair connects again, for example to use another project. Unpair stops the tunnel and disconnects this Home Assistant from Datum. Both restart the add-on." })
        : el("p", { class: "muted small", text: p.why_not || "" }),
      el("button", { class: "secondary", disabled: busy || !p.can_forget, onclick: function () { forget(false, p); } }, "Re-pair"),
      el("button", { class: "danger", disabled: busy || !p.can_forget, onclick: function () { forget(true, p); } }, "Unpair")));
    show.apply(null, parts);
  }

  function forget(unpair, p) {
    var question = unpair
      ? "Stop the tunnel and disconnect this Home Assistant from Datum? The add-on restarts and asks you to connect again. The service account stays in Datum until you delete it."
      : "Forget this add-on's key and connect to Datum again? The add-on restarts and shows the Connect button. The old service account stays in Datum until you delete it.";
    if (!window.confirm(question)) return;
    post(unpair ? "api/unpair" : "api/repair").then(function (r) {
      if (!r) return;
      actionResult = {
        title: unpair ? "Unpaired. Restarting…" : "Restarting to connect again…",
        text: "The add-on is restarting. This page reloads by itself when it is back.",
        account: r.service_account || p.service_account,
      };
      render(last);
      schedule(3000);
    }).catch(function (e) {
      render(last);
      root.appendChild(errorLine(e));
    });
  }

  function stateText(s) {
    return { online: "Online", starting: "Starting", offline: "Offline", off: "Off" }[s] || s;
  }

  function stateClass(s) {
    return { online: "ok", starting: "warn", offline: "error", off: "muted" }[s] || "";
  }

  function wafText(e) {
    if (!e) return "Checking…";
    if (e.waf === "missing") return "Off: no WAF policy";
    if (e.waf === "unknown") return "Could not be read";
    var text = "Datum WAF, " + (e.waf === "Enforce" ? "on" : e.waf === "Disabled" ? "off" : e.waf);
    if (e.waf_scoped) text += " (live views exempt)";
    return text;
  }

  function timeoutText(e) {
    if (!e) return "Checking…";
    if (e.timeout === "missing") return "15 seconds (default)";
    if (e.timeout === "unknown") return "Could not be read";
    return e.timeout;
  }

  // ---- The server went away ----

  function renderAway() {
    rendered = null;
    if (actionResult) return;
    if (last && last.mode === "setup" && last.setup.phase === "done") {
      show(card(
        el("h2", { text: "Starting the tunnel…" }),
        el("p", { class: "muted", text: "This page updates by itself." }),
        el("p", { class: "note", text: NEW_ADDRESS_NOTE })));
    } else {
      show(card(el("p", { class: "muted", text: "Waiting for the add-on… If this lasts, check that it is running, and its Log tab." })));
    }
  }

  setInterval(tickCountdown, 1000);
  poll();
})();
