// Subframes do not receive the main page's Tauri/event injection on WebKit.
// Forward only known external destinations; native internal/auth/blank popup
// behavior, including its WindowProxy return value, stays with the frame.
(function () {
  if (window === window.top) return;
  const config = window.pakeConfig || {};
  const isInternalUrl = createInternalUrlMatcher(config.internal_url_regex);
  const originalOpen = window.open;

  function externalDestination(rawUrl, name) {
    if (config.force_internal_navigation === true) return null;
    if (
      typeof rawUrl !== "string" ||
      !rawUrl.trim() ||
      rawUrl.trim().startsWith("#")
    ) {
      return null;
    }
    try {
      const url = new URL(rawUrl, document.baseURI);
      if (!["http:", "https:", "mailto:", "tel:"].includes(url.protocol))
        return null;
      if (window.isAuthPopup(url.href, name)) return null;
      if (isInternalUrl(url.href, config.url)) return null;
      return url.href;
    } catch (error) {
      return null;
    }
  }

  function forward(url) {
    window.top.postMessage({ type: "pake:frame-external-link", url }, "*");
  }

function createDeferredPopupProxy(name) {
    let closed = false;
    const navigate = (nextUrl) => {
      if (closed || typeof nextUrl !== "string" || !nextUrl.trim()) return;
      const external = externalDestination(nextUrl, name);
      if (external) {
        forward(external);
      } else {
        originalOpen.call(window, nextUrl, name || "_blank");
      }
    };
    const locationObj = {
      get href() {
        return "about:blank";
      },
      set href(nextUrl) {
        navigate(nextUrl);
      },
      assign(nextUrl) {
        navigate(nextUrl);
      },
      replace(nextUrl) {
        navigate(nextUrl);
      },
      toString() {
        return "about:blank";
      },
    };
    const target = {
      location: locationObj,
      opener: window,
      get closed() {
        return closed;
      },
      focus() {},
      blur() {},
      close() {
        closed = true;
      },
    };
    return new Proxy(target, {
      set(obj, prop, value) {
        if (prop === "location") {
          navigate(value);
          return true;
        }
        obj[prop] = value;
        return true;
      },
    });
  }

  window.open = function (url, name, specs) {
    url = normalizePopupUrl(url);
    // Named targets can navigate an existing frame and must retain its proxy.
    if (name && String(name).toLowerCase() !== "_blank") {
      return originalOpen.call(window, url, name, specs);
    }
    const external = externalDestination(url, name);
    if (external) {
      forward(external);
      return null;
    }
    const popup = originalOpen.call(window, url, name, specs);
    if (popup) return popup;
    if (!url || /^about:blank(?:[?#]|$)/i.test(String(url))) {
      return createDeferredPopupProxy(name);
    }
    return null;
  };

  window.addEventListener("click", (event) =>
    handleProtocolLinkClick(event, forward),
  );

  document.addEventListener(
    "click",
    (event) => {
      const anchor = event.target?.closest?.("a[href]");
      if (!anchor || anchor.hasAttribute("download")) return;
      // Named browsing contexts may be part of the page's own frame layout.
      if (anchor.target && !["_blank", "_new", "_self"].includes(anchor.target))
        return;
      const external = externalDestination(anchor.getAttribute("href"), "");
      if (!external || /^(mailto|tel):/.test(external)) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      forward(external);
    },
    true,
  );
})();
