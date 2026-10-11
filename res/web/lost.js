/* LOST web UI (ARCHITECTURE.md §10.3).
 *
 * One page that never reloads: search as you type, autocomplete, infinite
 * scrolling, and `cache:` (a page's stored text). Talks only to this
 * server's /api/find, /api/suggest and /api/page, and renders with Mikado. Page text is always escaped before it's shown; the
 * only markup added is <mark> around the matches the server points out.
 */
(function () {
  "use strict";

  const PAGE = 20; // results per page, and per request while scrolling
  const CHUNK = 100; // most results per request when opening a later page
  const MAX_PAGE = 250; // furthest page a link can open on
  const RESERVED = MAX_PAGE * PAGE; // results the scroll bar makes room for
  const TOP_AFTER = 100; // px scrolled before the back-to-top button shows
  const SEARCH_DELAY = 200; // ms after the last keystroke
  const SUGGEST_DELAY = 80;
  const SUGGESTIONS = 8;

  const body = document.body;
  const form = document.getElementById("search");
  const input = document.getElementById("q");
  const list = document.getElementById("suggestions");
  const status = document.getElementById("status");
  const more = document.getElementById("more");
  const header = document.querySelector("header");
  const logo = header.querySelector(".logo");
  const tagline = header.querySelector(".tagline");
  const main = document.querySelector("main");

  // ── Escaping and highlighting ─────────────────────────────────────────

  const ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

  function escape(text) {
    return String(text).replace(/[&<>"']/g, (c) => ESCAPES[c]);
  }

  // `ranges` are [start, end) offsets in characters (code points), as the
  // server sends them; Array.from splits a string the same way.
  function highlight(text, ranges) {
    if (!text) return "";
    if (!ranges || !ranges.length) return escape(text);
    const chars = Array.from(text);
    let html = "";
    let at = 0;
    for (const [start, end] of ranges) {
      if (start < at || end <= start || start >= chars.length) continue;
      html += escape(chars.slice(at, start).join(""));
      html += "<mark>" + escape(chars.slice(start, end).join("")) + "</mark>";
      at = end;
    }
    return html + escape(chars.slice(at).join(""));
  }

  // ── Templates ─────────────────────────────────────────────────────────
  // Hand-written Mikado templates (the light build has no compiler). Dynamic
  // values are marked [""]; `fn` fills them, in document order.

  const resultTemplate = {
    name: "result",
    tpl: {
      tag: "article",
      class: "result",
      child: [
        { tag: "a", class: "title", attr: { href: [""], rel: "noopener noreferrer" }, html: [""] },
        { tag: "p", class: "snippet", html: [""] },
        {
          tag: "div",
          class: "where",
          child: [
            { tag: "a", class: "url", attr: { href: [""], rel: "noopener noreferrer" }, text: [""] },
            { tag: "a", class: "cached", attr: { href: [""] }, text: "Cached" },
          ],
        },
      ],
    },
    fn: [
      function (hit, state, index, p) {
        p[0]._a("href", safeHref(hit.url));
        p[0]._h(hit.title ? highlight(hit.title, hit.title_matches) : escape(hit.url));
        p[1]._h(highlight(hit.snippet, hit.snippet_matches));
        p[2]._a("href", safeHref(hit.url));
        p[3]._t(hit.url);
        p[4]._a("href", cacheFor(hit.url, query));
      },
    ],
  };

  const suggestionTemplate = {
    name: "suggestion",
    tpl: { tag: "li", attr: { role: "option" }, text: [""] },
    fn: [
      function (text, state, index, p) {
        p[0]._t(text);
      },
    ],
  };

  // Only web links are clickable; anything else (javascript:, data:, …) isn't.
  function safeHref(url) {
    return /^(https?|gemini|gopher|ftp):\/\//i.test(url) ? url : "#";
  }

  const results = new Mikado(resultTemplate, { root: document.getElementById("results") });
  const suggestions = new Mikado(suggestionTemplate, { root: list });

  // ── Start page ⇄ results ──────────────────────────────────────────────
  // There's only ever this one page. Going from the start page to results
  // (and back), the logo and search box glide to their new places: each is
  // measured before and after the layout changes, then animated from the
  // old box to the new one ("FLIP"). Interrupted halfway, the next change
  // carries on from wherever things are on screen.

  const MORPH_TIME = 320; // ms
  const MORPH_EASING = "cubic-bezier(0.2, 0.8, 0.2, 1)";
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  let morphs = []; // animations in flight
  let morphDone = []; // tidy-ups to run when they stop

  function stopMorph() {
    const done = morphDone;
    morphDone = [];
    morphs.forEach((a) => a.cancel());
    morphs = [];
    done.forEach((f) => f());
  }

  function play(el, keyframes, options, done) {
    const a = el.animate(keyframes, Object.assign({ duration: MORPH_TIME, easing: MORPH_EASING }, options));
    morphs.push(a);
    if (done) morphDone.push(done);
    a.onfinish = () => {
      morphs = morphs.filter((m) => m !== a);
      if (done) {
        morphDone = morphDone.filter((f) => f !== done);
        done();
      }
      a.cancel(); // drop any fill; the stylesheet has the final look
    };
  }

  // Once the start page is fully back, the old results go.
  function clearResults() {
    if (!body.classList.contains("home")) return;
    results.clear();
    status.textContent = "";
    updateRest();
  }

  function setHome(home, animate) {
    if (home) body.classList.remove("has-results");
    if (body.classList.contains("home") === home) return;

    if (!animate || reduceMotion.matches || !Element.prototype.animate) {
      stopMorph();
      body.classList.toggle("home", home);
      if (home) clearResults();
      return;
    }

    // First: where things are now, mid-animation or not.
    const first = {
      logo: logo.getBoundingClientRect(),
      form: form.getBoundingClientRect(),
      tagline: tagline.getBoundingClientRect(),
      header: header.getBoundingClientRect(),
      main: main.getBoundingClientRect(),
    };
    const taglineOpacity = parseFloat(getComputedStyle(tagline).opacity) || 0;
    // Hidden on the start page (where it would claim to be fully opaque).
    const mainOpacity = home ? parseFloat(getComputedStyle(main).opacity) || 0 : 0;
    const borderColor = getComputedStyle(header).borderBottomColor;

    stopMorph();
    if (home) window.scrollTo(0, 0);
    body.classList.toggle("home", home);

    // Last: where the new layout puts them.
    const last = {
      logo: logo.getBoundingClientRect(),
      form: form.getBoundingClientRect(),
      tagline: tagline.getBoundingClientRect(),
    };
    const shift = (a, b) => "translate(" + (a.left - b.left) + "px, " + (a.top - b.top) + "px)";

    // The logo moves and changes size, a little quicker than the box moves,
    // so the box never runs into it on the way up.
    const scale = first.logo.width / last.logo.width;
    play(logo, [{ transform: shift(first.logo, last.logo) + " scale(" + scale + ")" }, { transform: "none" }], {
      duration: MORPH_TIME * 0.75,
    });

    // The search box moves and changes width. On the start page it's centred,
    // so it's moved by its centre; in the bar it grows from its left edge.
    const fromX = home
      ? first.form.left + first.form.width / 2 - (last.form.left + last.form.width / 2)
      : first.form.left - last.form.left;
    const formFrame = (width, transform) => ({
      flexGrow: 0,
      flexShrink: 0,
      flexBasis: "auto",
      width: width + "px",
      transform: transform,
    });
    // Where the box ends up is measured again once it's there (mid-way its
    // width is still the start page's).
    play(
      form,
      [
        formFrame(first.form.width, "translate(" + fromX + "px, " + (first.form.top - last.form.top) + "px)"),
        formFrame(last.form.width, "none"),
      ],
      {},
      alignResults
    );

    // The tagline fades out as it goes up, and back in as it comes down.
    const visible = { visibility: "visible" };
    play(
      tagline,
      home
        ? [
            Object.assign({ transform: shift(first.tagline, last.tagline), opacity: taglineOpacity }, visible),
            Object.assign({ opacity: 0, offset: 0.4 }, visible),
            Object.assign({ transform: "none", opacity: 1 }, visible),
          ]
        : [
            Object.assign({ transform: shift(first.tagline, last.tagline), opacity: taglineOpacity }, visible),
            Object.assign({ opacity: 0, offset: 0.5 }, visible),
            Object.assign({ transform: "none", opacity: 0 }, visible),
          ]
    );

    // The bar's bottom edge fades in or out.
    play(header, [{ borderBottomColor: borderColor }, { borderBottomColor: getComputedStyle(header).borderBottomColor }]);

    if (!home) {
      // The results fade in under the bar.
      play(main, [{ opacity: mainOpacity }, { opacity: 1 }], { delay: MORPH_TIME * 0.25, fill: "backwards" });
      return;
    }

    // Going back: the start page hides the results at once, so a stand-in
    // stays where they were (below the bar, as scrolled) and fades away.
    const top = Math.max(first.main.top, first.header.bottom);
    if (first.main.height > 0 && top < window.innerHeight) {
      Object.assign(main.style, {
        display: "block",
        position: "fixed",
        zIndex: "1",
        top: top + "px",
        left: first.main.left + "px",
        width: first.main.width + "px",
        height: window.innerHeight - top + "px",
        margin: "0",
        overflow: "hidden",
        pointerEvents: "none",
      });
      main.scrollTop = top - first.main.top;
      play(main, [{ opacity: mainOpacity }, { opacity: 0 }], { duration: MORPH_TIME * 0.6, easing: "ease-out", fill: "forwards" }, () => {
        main.removeAttribute("style");
        clearResults();
      });
    } else {
      clearResults();
    }
  }

  // ── Search ────────────────────────────────────────────────────────────

  let query = "";
  let offset = 0; // results loaded so far
  let total = 0;
  let loading = false;
  let generation = 0; // drops responses to queries that have since changed
  let started = false; // a link straight to results opens on them, unanimated
  let page = 1; // the page at the top of the window, as in the address bar (&p=)
  let hl = ""; // with cache:, the search whose words to mark (&hl=)
  let queryHl = ""; // … as shown now

  const resultsRoot = document.getElementById("results");
  const spinner = document.getElementById("loading");

  function find(q, from, count) {
    return fetch("/api/find?q=" + encodeURIComponent(q) + "&l=" + from + "," + count).then((r) => {
      if (!r.ok) throw new Error("HTTP " + r.status);
      return r.json();
    });
  }

  // Results [from, to), in requests of at most CHUNK. Resolves to the first
  // response with all the results in it, or null if the query has changed.
  async function fetchRange(q, from, to, mine) {
    let data = null;
    const all = [];
    while (from + all.length < to) {
      const next = await find(q, from + all.length, Math.min(to - from - all.length, CHUNK));
      if (mine !== generation) return null;
      if (!data) data = next;
      all.push(...next.results);
      if (!next.results.length || from + all.length >= next.total) break;
    }
    data.results = all;
    return data;
  }

  // The bouncing dots: where the results will appear while a search runs
  // (with any old results dimmed below), after them while more load.
  function showLoading(where) {
    if (where === "top") main.insertBefore(spinner, resultsRoot);
    else rest.prepend(spinner);
    spinner.hidden = false;
  }

  function hideLoading() {
    spinner.hidden = true;
    resultsRoot.classList.remove("stale");
  }

  function search(q, history, p) {
    q = q.trim();
    if (q === query && hl === queryHl && history !== "force") return;
    query = q;
    queryHl = hl;
    offset = 0;
    total = 0;
    pendingPage = 0;
    page = p > 1 ? p : 1;
    const mine = ++generation;
    updateUrl(urlFor(q, page), history);
    updateTop();

    if (!q) {
      // An emptied box isn't a way back to the start page (the logo is):
      // the results layout stays, with nothing in it.
      document.title = "LOST";
      loading = false;
      hideLoading();
      status.textContent = "";
      pageView.hidden = true;
      results.clear();
      body.classList.remove("has-results");
      updateRest();
      window.scrollTo(0, 0);
      return;
    }
    const wasHome = body.classList.contains("home");
    setHome(false, started);
    alignResults();
    // The start page has no suggestions: now there's room for them.
    if (wasHome && started && document.activeElement === input) suggest(input.value);
    document.title = q + " – LOST";
    loading = true;
    status.textContent = "";
    if (CACHE.test(q)) return showPage(q, mine);
    pageView.hidden = true;
    resultsRoot.classList.add("stale");
    showLoading("top");

    fetchRange(q, 0, page * PAGE, mine)
      .then((data) => {
        if (!data) return;
        hideLoading();
        total = data.total;
        offset = data.results.length;
        results.render(data.results);
        body.classList.toggle("has-results", data.results.length > 0);
        status.textContent = summary(data);
        updateRest();
        scrollToPage(page);
      })
      .catch(() => {
        if (mine !== generation) return;
        hideLoading();
        results.clear();
        body.classList.remove("has-results");
        updateRest();
        status.textContent = "The LOST server isn't responding.";
      })
      .finally(() => {
        if (mine === generation) loading = false;
        keepLoading();
      });
  }

  // ── Stored pages (cache:) ─────────────────────────────────────────────
  // `cache:URL` shows the page's text as LOST keeps it, in place of the
  // results: plain text only, like a text-mode browser (the HTML was never
  // stored). Opened from a result's "Cached" link, the search's words are
  // marked (&hl=).

  const CACHE = /^cache:/i;
  const pageView = document.getElementById("page");
  const pageTitle = pageView.querySelector("h1");
  const pageUrl = pageView.querySelector(".meta .url");
  const pageMeta = pageView.querySelector(".meta span");
  const pageText = pageView.querySelector(".text");

  function cacheFor(url, words) {
    return "/?q=" + encodeURIComponent("cache:" + url) + (words ? "&hl=" + encodeURIComponent(words) : "");
  }

  function hostOf(url) {
    try {
      return new URL(/:\/\//.test(url) ? url : "https://" + url).hostname;
    } catch (e) {
      return "";
    }
  }

  // A status line with a link that searches `q` (inside this page).
  function statusWithSearch(text, label, q) {
    status.textContent = text + " ";
    const a = document.createElement("a");
    a.href = urlFor(q, 1);
    a.className = "internal";
    a.textContent = label;
    status.append(a);
  }

  function showPage(q, mine) {
    const url = q.replace(CACHE, "").trim();
    results.clear();
    body.classList.remove("has-results");
    pageView.hidden = true;
    updateRest();
    window.scrollTo(0, 0);
    if (!url || /\s/.test(url)) {
      loading = false;
      hideLoading();
      status.textContent = "cache: takes one address on its own, like cache:https://example.com/page";
      return;
    }
    showLoading("top");
    fetch("/api/page?url=" + encodeURIComponent(url) + (hl ? "&hl=" + encodeURIComponent(hl) : ""))
      .then((r) => {
        if (r.status === 404) return null;
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.json();
      })
      .then((stored) => {
        if (mine !== generation) return;
        hideLoading();
        if (!stored) {
          const host = hostOf(url);
          if (host) statusWithSearch("This page isn't in the database.", "Search site:" + host, "site:" + host);
          else status.textContent = "This page isn't in the database.";
          return;
        }
        status.textContent = "";
        pageTitle.innerHTML = stored.title ? highlight(stored.title, stored.title_matches) : escape(stored.url);
        pageUrl.href = safeHref(stored.url);
        pageUrl.textContent = stored.url;
        pageMeta.textContent = stored.crawled ? "Captured " + stored.crawled : "";
        pageMeta.hidden = !stored.crawled;
        pageText.innerHTML = highlight(stored.text, stored.text_matches);
        pageView.hidden = false;
        document.title = (stored.title || stored.url) + " – LOST";
      })
      .catch(() => {
        if (mine !== generation) return;
        hideLoading();
        status.textContent = "The LOST server isn't responding.";
      })
      .finally(() => {
        if (mine === generation) loading = false;
        updateTop();
      });
  }

  // Links within the page (a result's "Cached", "Search site:…") open
  // here, without reloading; with a modifier key, wherever the browser
  // puts them.
  document.addEventListener("click", (e) => {
    const a = e.target.closest && e.target.closest("a.cached, a.internal");
    if (!a || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    const params = new URL(a.href, location.href).searchParams;
    const q = params.get("q") || "";
    clearTimeout(searchTimer);
    clearSuggestions();
    input.value = q;
    hl = params.get("hl") || "";
    search(q, "push");
  });

  // The start page: from the logo, or Back to a "/" address.
  function goHome(history) {
    query = "";
    offset = 0;
    total = 0;
    pendingPage = 0;
    page = 1;
    loading = false;
    ++generation;
    updateUrl("/", history);
    document.title = "LOST";
    hideLoading();
    pageView.hidden = true;
    hl = queryHl = "";
    setHome(true, started);
    updateTop();
  }

  // Loads the next results: a page, or as many as it takes to reach result
  // `to`, in requests of at most CHUNK so they show up as they come.
  function loadMore(to) {
    if (loading || !query || offset >= total) return;
    const mine = generation;
    const count = Math.min(CHUNK, Math.max(PAGE, Math.ceil(((to || 0) - offset) / PAGE) * PAGE));
    loading = true;
    showLoading("end");
    find(query, offset, count)
      .then((data) => {
        if (mine !== generation) return;
        offset += data.results.length;
        if (!data.results.length) offset = total; // nothing more after all
        results.append(data.results);
        updateRest();
      })
      .catch(() => {})
      .finally(() => {
        if (mine !== generation) return;
        loading = false;
        hideLoading();
        keepLoading();
      });
  }

  // After each load: on towards a page being opened, or whatever part of
  // the results the window has reached.
  function keepLoading() {
    if (pendingPage) {
      if (offset > (pendingPage - 1) * PAGE || offset >= total) {
        const p = pendingPage;
        pendingPage = 0;
        scrollToPage(p);
      } else {
        return loadMore(pendingPage * PAGE);
      }
    }
    const to = reached();
    if (to > offset) loadMore(to);
  }

  function summary(data) {
    if (!data.total) return "Found no results for “" + data.query + "”";
    const n = data.total.toLocaleString("en-US");
    return "Found " + n + " result" + (data.total === 1 ? "" : "s") + " (" + data.took_ms + " ms)";
  }

  // ── Room for the rest ─────────────────────────────────────────────────
  // Below the loaded results, empty space stands in for those not loaded
  // yet (as tall as they're likely to be), so the scroll bar shows the whole
  // list from the start, and barely moves as results arrive. Dragging it
  // far down loads everything up to there. Beyond RESERVED results, the
  // list just grows as you scroll.

  const rest = document.getElementById("rest");

  // Average height of a result so far, margins included.
  function resultHeight() {
    const items = resultsRoot.children;
    if (!items.length) return 0;
    return (more.getBoundingClientRect().top - items[0].getBoundingClientRect().top) / items.length;
  }

  function updateRest() {
    const missing = Math.max(0, Math.min(total, RESERVED) - offset);
    rest.style.minHeight = missing && query ? Math.round(missing * resultHeight()) + "px" : "";
  }

  // The result the window (plus a margin) reaches down to, counting those
  // the empty space stands for.
  function reached() {
    if (!query || offset >= total) return 0;
    const below = window.innerHeight + 600 - rest.getBoundingClientRect().top;
    if (below <= 0) return 0;
    return offset + Math.ceil(below / (resultHeight() || 100));
  }

  // Infinite scrolling: fetch more when the end of the list shows up.
  const observer = new IntersectionObserver((entries) => entries.some((e) => e.isIntersecting) && keepLoading(), {
    rootMargin: "600px 0px",
  });
  observer.observe(more);

  window.addEventListener("resize", () => updateRest());

  // ── Pages ─────────────────────────────────────────────────────────────
  // Results come in pages of PAGE. The address bar says which page is at
  // the top of the window (&p=4; nothing for page 1), so a link can bring
  // someone back to the same spot. It's the same numbering paged, no-JS
  // pages would use.

  let pendingPage = 0; // a page to show once it's loaded

  // Puts the first result of page `p` just under the bar (page 1: the top).
  function scrollToPage(p) {
    const items = resultsRoot.children;
    p = Math.min(p, Math.ceil(items.length / PAGE)); // past the end: the last page
    const first = items[(p - 1) * PAGE];
    if (p <= 1 || !first) {
      window.scrollTo(0, 0);
    } else {
      const y = first.getBoundingClientRect().top + window.scrollY - header.offsetHeight - 12;
      window.scrollTo(0, Math.max(0, y));
    }
    trackPage();
  }

  // Back/Forward within the same search.
  function goToPage(p) {
    if ((p - 1) * PAGE < offset || offset >= total) return scrollToPage(p);
    pendingPage = p;
    keepLoading();
  }

  function trackPage() {
    if (!pageView.hidden) {
      drawRing();
      return updateTop();
    }
    const items = resultsRoot.children;
    // While a new search loads, the results on screen are the old ones:
    // where they're scrolled to says nothing about the new search's page.
    const replacing = loading && offset === 0;
    if (query && !body.classList.contains("home") && items.length && !replacing) {
      findPage(items);
      drawRing();
    }
    updateTop();
  }

  function findPage(items) {
    // The result at the top: the first one not hidden under the bar.
    const line = header.getBoundingClientRect().bottom + 1;
    let lo = 0;
    let hi = items.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (items[mid].getBoundingClientRect().bottom <= line) lo = mid + 1;
      else hi = mid;
    }
    if (items[lo].getBoundingClientRect().bottom <= line) {
      // Down in the room for results not loaded yet: the one that will be there.
      const into = Math.max(0, line - rest.getBoundingClientRect().top);
      lo = Math.min(total - 1, items.length + Math.floor(into / (resultHeight() || 100)));
    }
    const p = Math.floor(lo / PAGE) + 1;
    if (p !== page) {
      page = p;
      updateUrl(urlFor(query, page), "replace");
    }
  }

  // ── Back to the top ───────────────────────────────────────────────────
  // Once scrolled a little, a button in the corner goes back to the top. The ring
  // around it is the scroll bar's position, which (with the room kept for
  // the rest) is how far through the results the window is: full only at
  // the very bottom. Results past RESERVED count as if they were there.

  const topButton = document.getElementById("top");
  const ring = topButton.querySelector(".ring circle");
  // The ring's length, in the units it's drawn in. (Worked out here rather
  // than with pathLength, which Safari applies differently once scaled.)
  const RING = 2 * Math.PI * ring.r.baseVal.value;

  function drawRing() {
    const unreserved = Math.max(0, total - Math.max(offset, Math.min(total, RESERVED)));
    const height = document.documentElement.scrollHeight + unreserved * resultHeight();
    const range = height - window.innerHeight;
    const progress = range > 0 ? Math.min(1, Math.max(0, window.scrollY / range)) : 1;
    ring.setAttribute("stroke-dashoffset", (RING * (1 - progress)).toFixed(2));
  }

  // Shown once scrolled a little, so there's always a sign of where you are
  // (macOS hides scroll bars when they're not moving).
  function updateTop() {
    const show = query !== "" && !body.classList.contains("home") && window.scrollY > TOP_AFTER;
    topButton.classList.toggle("shown", show);
    topButton.tabIndex = show ? 0 : -1;
  }

  // Scrolled by hand rather than with the browser's smooth scrolling, which
  // can stop short when the page changes under it (more results arriving).
  let scrolling = 0;

  function stopScrolling() {
    cancelAnimationFrame(scrolling);
    scrolling = 0;
  }

  topButton.addEventListener("click", () => {
    stopScrolling();
    const from = window.scrollY;
    if (reduceMotion.matches || from < 2) return window.scrollTo(0, 0);
    const start = performance.now();
    const duration = Math.min(700, 250 + from / 20);
    const step = (now) => {
      const t = Math.min(1, (now - start) / duration);
      window.scrollTo(0, from * Math.pow(1 - t, 3)); // eases out
      scrolling = t < 1 ? requestAnimationFrame(step) : 0;
    };
    scrolling = requestAnimationFrame(step);
  });

  // Scrolling by hand takes over.
  for (const type of ["wheel", "touchstart", "keydown"]) {
    window.addEventListener(type, () => scrolling && stopScrolling(), { passive: true });
  }

  let tracking = false;
  window.addEventListener(
    "scroll",
    () => {
      if (tracking) return;
      tracking = true;
      requestAnimationFrame(() => {
        tracking = false;
        trackPage();
        if (!loading) keepLoading();
      });
    },
    { passive: true }
  );

  // ── Address bar and history ───────────────────────────────────────────

  if ("scrollRestoration" in window.history) window.history.scrollRestoration = "manual";

  // "/" is the start page; "/?q=" (even empty) is the results page.
  function urlFor(q, p) {
    if (CACHE.test(q.trim())) return cacheFor(q.trim().replace(CACHE, ""), hl);
    return "/?q=" + encodeURIComponent(q) + (q && p > 1 ? "&p=" + p : "");
  }

  function updateUrl(url, history) {
    if (location.pathname + location.search === url) return;
    if (history === "push") window.history.pushState(null, "", url);
    else if (history === "replace") window.history.replaceState(null, "", url);
  }

  // q is null on the start page (no q at all).
  function fromUrl() {
    const params = new URLSearchParams(location.search);
    const p = parseInt(params.get("p"), 10);
    return { q: params.get("q"), p: p >= 1 ? Math.min(p, MAX_PAGE) : 1, hl: params.get("hl") || "" };
  }

  window.addEventListener("popstate", () => {
    const { q, p, hl: words } = fromUrl();
    input.value = q || "";
    clearSuggestions();
    hl = words;
    if (q === null) goHome("none");
    else if (q.trim() && q.trim() === query && hl === queryHl && !CACHE.test(q.trim())) goToPage(p);
    else {
      search(q, "none", p);
      setHome(false, started); // "/?q=" with nothing in it still isn't the start page
      alignResults();
    }
  });

  // ── Suggestions ───────────────────────────────────────────────────────
  // Pills beside (or under) the search box, never over the results. None
  // on the start page: they start once the page has turned into results.

  let active = -1;
  let suggestGeneration = 0;

  function showSuggestions(items) {
    active = -1;
    suggestions.render(items);
    input.setAttribute("aria-expanded", items.length ? "true" : "false");
  }

  function clearSuggestions() {
    suggestGeneration++;
    showSuggestions([]);
  }

  function suggest(text) {
    const mine = ++suggestGeneration;
    if (!text.trim()) return clearSuggestions();
    if (body.classList.contains("home")) return;
    fetch("/api/suggest?q=" + encodeURIComponent(text))
      .then((r) => (r.ok ? r.json() : []))
      .then((items) => {
        if (mine !== suggestGeneration || input.value !== text) return;
        showSuggestions(items.filter((s) => s !== text.trim()).slice(0, SUGGESTIONS));
      })
      .catch(() => {});
  }

  // The pills on the visible line (the others wrapped out of sight).
  function visiblePills() {
    const items = Array.from(list.children);
    return items.filter((li) => li.offsetTop === items[0].offsetTop);
  }

  function setActive(index) {
    const items = visiblePills();
    if (!items.length) return;
    if (active >= 0 && items[active]) {
      items[active].classList.remove("active");
      items[active].removeAttribute("aria-selected");
    }
    active = (index + items.length) % items.length;
    items[active].classList.add("active");
    items[active].setAttribute("aria-selected", "true");
    replaceText(items[active].textContent);
  }

  // Puts `text` in the box the way typing would, so Ctrl+Z can undo it
  // (setting the value directly would wipe the undo history).
  let typing = true;
  function replaceText(text) {
    input.focus();
    input.select();
    typing = false;
    const done = document.execCommand && document.execCommand("insertText", false, text);
    typing = true;
    if (!done || input.value !== text) input.value = text;
  }

  function choose(text) {
    replaceText(text);
    clearTimeout(searchTimer);
    search(text, "push");
    suggest(text); // and refinements of it
  }

  // mousedown, not click: the box keeps its focus.
  list.addEventListener("mousedown", (e) => {
    const li = e.target.closest("li");
    if (!li) return;
    e.preventDefault();
    input.focus();
    choose(li.textContent);
  });

  // ── Typing ────────────────────────────────────────────────────────────

  let searchTimer = 0;
  let suggestTimer = 0;

  input.addEventListener("input", (e) => {
    if (!typing) return; // our own replaceText
    // Undo/redo of a picked suggestion would leave it all selected (the
    // selection replaceText made); put the cursor at the end instead.
    if (e.inputType === "historyUndo" || e.inputType === "historyRedo") {
      const end = input.value.length;
      input.setSelectionRange(end, end);
    }
    const text = input.value;
    hl = ""; // typed by hand: nothing carried over to mark
    clearTimeout(searchTimer);
    clearTimeout(suggestTimer);
    searchTimer = setTimeout(() => search(text, "replace"), SEARCH_DELAY);
    suggestTimer = setTimeout(() => suggest(text), SUGGEST_DELAY);
  });

  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" && list.children.length) {
      e.preventDefault();
      setActive(active + 1);
    } else if (e.key === "ArrowUp" && list.children.length) {
      e.preventDefault();
      setActive(active - 1);
    } else if (e.key === "Escape" && active >= 0) {
      e.preventDefault();
      const items = visiblePills();
      if (items[active]) {
        items[active].classList.remove("active");
        items[active].removeAttribute("aria-selected");
      }
      active = -1;
    }
  });

  // Pills are shown only while the box is in use, so they don't distract
  // from the results.
  let quietFocus = false;
  input.addEventListener("focus", () => {
    list.classList.remove("idle");
    // None yet (a link straight to results, say): fetch them now.
    if (!quietFocus && !list.children.length && input.value.trim()) suggest(input.value);
  });
  input.addEventListener("blur", () => list.classList.add("idle"));

  // The logo goes back to the start page, without reloading.
  logo.addEventListener("click", (e) => {
    if (e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    clearTimeout(searchTimer);
    clearTimeout(suggestTimer);
    clearSuggestions();
    input.value = "";
    goHome("push");
    input.focus();
  });

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    clearTimeout(searchTimer);
    search(input.value, "push");
  });

  // "/" focuses the search box, as on many sites.
  document.addEventListener("keydown", (e) => {
    if (e.key === "/" && document.activeElement !== input) {
      e.preventDefault();
      input.focus();
      input.select();
    }
  });

  // ── Layout ────────────────────────────────────────────────────────────

  // Results start where the search box does, whatever the logo's size.
  function alignResults() {
    if (body.classList.contains("home")) return;
    document.documentElement.style.setProperty("--indent", form.offsetLeft + "px");
  }
  window.addEventListener("resize", alignResults);

  // ── Start ─────────────────────────────────────────────────────────────

  const initial = fromUrl();
  if (initial.q !== null) {
    input.value = initial.q;
    hl = initial.hl;
    search(initial.q, "none", initial.p);
    setHome(false, false);
    alignResults();
  }
  // Ready to type, without opening suggestions for a query just opened.
  quietFocus = true;
  input.focus();
  quietFocus = false;
  started = true;
})();
