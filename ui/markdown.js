/*
 * DBX Notes Markdown bridge.
 *
 * Uses markdown-it (CommonMark + GFM tables/strikethrough) for the
 * Markdown -> HTML direction, with small adapters that keep the DOM contract
 * expected by the existing live editor:
 *
 *   - task lists: <li class="task"><input type="checkbox"><span>…</span></li>
 *   - attachment:<id> links: <a href="#" data-attachment-id="…">
 *   - data:/dbx-attachment: links: <a href="#" data-download-url="…">
 *   - image src="dbx-attachment:<id>" is left for the caller to resolve
 *
 * The HTML -> Markdown serializer stays in index.html on purpose; changing
 * both directions at once would make regressions much harder to isolate.
 */
(function () {
  "use strict";

  if (typeof window.markdownit !== "function") return;

  var md = window.markdownit({
    html: false,       // raw HTML is escaped, matching the old inlineMarkdown()
    breaks: true,      // soft line breaks become <br>, matching the old parser
    linkify: false,    // bare URLs were never auto-linked
    typographer: false
  });

  // The old parser accepted http(s), mailto, data:, attachment: and
  // dbx-attachment:. Keep exactly that set, plus relative URLs.
  md.validateLink = function (url) {
    var value = String(url == null ? "" : url).trim();
    if (/^(vbscript|javascript|file):/i.test(value)) return false;
    if (/^(https?:|mailto:|data:|attachment:|dbx-attachment:)/i.test(value)) return true;
    return !/^[a-z][a-z0-9+.-]*:/i.test(value);
  };

  // Task list marker -> the DOM shape the live editor already understands.
  md.core.ruler.after("inline", "dbx-task-lists", function (state) {
    var tokens = state.tokens;
    for (var i = 2; i < tokens.length; i += 1) {
      var inline = tokens[i];
      var paragraph = tokens[i - 1];
      var item = tokens[i - 2];
      if (!inline || inline.type !== "inline") continue;
      if (!paragraph || paragraph.type !== "paragraph_open") continue;
      if (!item || item.type !== "list_item_open") continue;
      var match = /^\[([ xX])\]\s+/.exec(inline.content);
      if (!match) continue;

      inline.content = inline.content.slice(match[0].length);
      if (inline.children && inline.children.length && inline.children[0].type === "text") {
        inline.children[0].content = inline.children[0].content.slice(match[0].length);
      }

      var attrs = item.attrGet("class") || "";
      item.attrSet("class", (attrs + " task").trim());

      var checkbox = new state.Token("html_inline", "", 0);
      checkbox.content = '<input type="checkbox"' + (match[1].toLowerCase() === "x" ? " checked" : "") + ">";
      var open = new state.Token("html_inline", "", 0);
      open.content = "<span>";
      var close = new state.Token("html_inline", "", 0);
      close.content = "</span>";
      inline.children.unshift(checkbox, open);
      inline.children.push(close);
    }
  });

  var defaultLinkOpen = md.renderer.rules.link_open || function (tokens, idx, options, env, self) {
    return self.renderToken(tokens, idx, options);
  };
  md.renderer.rules.link_open = function (tokens, idx, options, env, self) {
    var token = tokens[idx];
    var href = token.attrGet("href") || "";
    // The old renderer stored the link label so downloads get a useful name.
    var label = "";
    for (var j = idx + 1; j < tokens.length && tokens[j].type !== "link_close"; j += 1) {
      if (tokens[j].type === "text" || tokens[j].type === "code_inline") label += tokens[j].content;
    }
    token.attrSet("rel", "noreferrer");
    if (href.indexOf("attachment:") === 0) {
      token.attrSet("href", "#");
      token.attrSet("data-attachment-id", href.slice("attachment:".length));
      token.attrSet("data-attachment-name", label);
    } else if (href.indexOf("dbx-attachment:") === 0 || href.indexOf("data:") === 0) {
      token.attrSet("href", "#");
      token.attrSet("data-download-url", href);
      token.attrSet("data-download-name", label);
    }
    return defaultLinkOpen(tokens, idx, options, env, self);
  };

  var defaultImage = md.renderer.rules.image || function (tokens, idx, options, env, self) {
    return self.renderToken(tokens, idx, options);
  };
  var imagesAsLinks = false;
  md.renderer.rules.image = function (tokens, idx, options, env, self) {
    if (imagesAsLinks) {
      // Lite has no attachment support and historically rendered every image
      // as a plain link. Keep that behaviour when the caller asks for it.
      var token = tokens[idx];
      var src = token.attrGet("src") || "";
      var alt = token.content || "";
      return '<a href="' + md.utils.escapeHtml(src) + '" rel="noreferrer">' + md.utils.escapeHtml(alt) + "</a>";
    }
    tokens[idx].attrSet("loading", "lazy");
    return defaultImage(tokens, idx, options, env, self);
  };

  function toHtml(source, options) {
    imagesAsLinks = !!(options && options.imagesAsLinks);
    return md.render(String(source == null ? "" : source));
  }

  window.DBXMarkdown = {
    toHtml: toHtml,
    engine: "markdown-it " + (md.version || "")
  };
})();
