// Save buttons: clicking one adds the entry's `system:saved` tag, or removes
// it if the entry is already saved, and updates the entry's tag list to match.
"use strict";

const SAVED_TAG = "system:saved";

document.addEventListener("click", async (event) => {
  const button = event.target.closest("button.save");
  if (!button || button.disabled) {
    return;
  }
  const save = button.getAttribute("aria-pressed") !== "true";
  button.disabled = true;
  try {
    const resp = await fetch(`/entries/${button.dataset.entry}/saved`, {
      method: save ? "PUT" : "DELETE",
    });
    if (!resp.ok) {
      throw new Error(`${resp.status} ${resp.statusText}`);
    }
    setSaved(button, save);
  } catch (e) {
    console.error("failed to update saved entry:", e);
    button.title = save ? "Could not save this entry" : "Could not unsave this entry";
  } finally {
    button.disabled = false;
  }
});

function setSaved(button, saved) {
  button.setAttribute("aria-pressed", String(saved));
  button.title = saved ? "Unsave" : "Save";

  // The entry's tags are listed alongside the button, system tags first and
  // sorted by name, so `saved` goes after any other system tags.
  const entry = button.closest("li, article");
  let tags = entry.querySelector(":scope > ul.tags");
  const existing = tags?.querySelector(`li.tag.system[title="${SAVED_TAG}"]`);
  if (!saved) {
    existing?.remove();
    if (tags && !tags.children.length) {
      tags.remove();
    }
    return;
  }
  if (existing) {
    return;
  }
  if (!tags) {
    tags = document.createElement("ul");
    tags.className = "tags";
    tags.setAttribute("aria-label", "Tags");
    // On an entry's page the tags come before its content; in a list of
    // entries, they come last.
    const content = entry.querySelector(":scope > .content");
    if (content) {
      content.before(tags);
    } else {
      entry.append(tags);
    }
  }
  const tag = document.createElement("li");
  tag.className = "tag system";
  tag.title = SAVED_TAG;
  tag.textContent = "saved";
  const systemTags = tags.querySelectorAll(":scope > li.tag.system");
  if (systemTags.length) {
    systemTags[systemTags.length - 1].after(tag);
  } else {
    tags.prepend(tag);
  }
}

// Filter menus: ticking or unticking a filter's checkbox loads the list with
// the filter changed, from the URL the page gives in its `data-href`.
document.addEventListener("change", (event) => {
  const toggle = event.target.closest("input.filter-toggle");
  if (toggle) {
    window.location.href = toggle.dataset.href;
  }
});

// Search boxes: submitting one loads the search page for what was typed. Pages
// may not submit forms themselves (their `Content-Security-Policy` says
// `form-action 'none'`), so the script goes there instead.
document.addEventListener("submit", (event) => {
  const form = event.target.closest("form.search");
  if (!form) {
    return;
  }
  event.preventDefault();
  const query = new FormData(form).get("q").trim();
  if (query) {
    // Close the search popup first, so that it is not still open if the
    // browser comes back to this page from its cache.
    form.closest("dialog")?.close();
    window.location.href = `/search?q=${encodeURIComponent(query)}`;
  }
});

// Search popup: on narrow screens, where the nav has no room for the search
// box, a button opens it in a popup instead. Its "Cancel" button, or a tap
// outside it, closes it again; so does Escape, as with any modal dialog.
// Closing it puts back what the box said before it was opened.
document.addEventListener("click", (event) => {
  if (event.target.closest("button.search-open")) {
    const dialog = document.querySelector("dialog.search-dialog");
    dialog.showModal();
    // Select what the box says, as on the search page, so that typing
    // replaces it rather than going in front of it.
    dialog.querySelector("input[name=q]").select();
    return;
  }
  const dialog = event.target.closest("dialog.search-dialog");
  // A tap on the backdrop is a click on the dialog itself; the form fills
  // the dialog, so taps inside it land on the form or its children.
  if (dialog && (event.target === dialog || event.target.closest("button.search-close"))) {
    dialog.close();
  }
});

// Escape closes the popup at once. Left to itself, a search box that has
// text in it — as it does on the search page — would clear it instead, and
// only a second Escape would close the popup.
document.addEventListener("keydown", (event) => {
  const dialog = event.target.closest?.("dialog.search-dialog");
  if (dialog && event.key === "Escape") {
    event.preventDefault();
    dialog.close();
  }
});

// `close` does not bubble, so it is caught on its way down instead.
document.addEventListener(
  "close",
  (event) => {
    if (event.target.matches("dialog.search-dialog")) {
      event.target.querySelector("form").reset();
    }
  },
  true,
);

// "Mark all as read" buttons: clicking one, once confirmed, marks every entry
// (or every entry from the button's feed) as read, and reloads the page.
document.addEventListener("click", async (event) => {
  const button = event.target.closest("button.mark-read");
  if (!button || button.disabled) {
    return;
  }
  const feed = button.dataset.feed;
  const what = feed ? "every entry from this feed" : "every entry";
  if (!confirm(`Mark ${what} as read?`)) {
    return;
  }
  button.disabled = true;
  try {
    const query = feed ? `?feed=${encodeURIComponent(feed)}` : "";
    const resp = await fetch(`/entries/read${query}`, { method: "POST" });
    if (!resp.ok) {
      throw new Error(`${resp.status} ${resp.statusText}`);
    }
    location.reload();
  } catch (e) {
    console.error("failed to mark entries as read:", e);
    button.title = "Could not mark the entries as read";
    button.disabled = false;
  }
});

// "Delete tag" buttons: clicking one, once confirmed, deletes the button's
// tag and goes back to the list of tags.
document.addEventListener("click", async (event) => {
  const button = event.target.closest("button.delete-tag");
  if (!button || button.disabled) {
    return;
  }
  const name = button.dataset.name;
  if (!confirm(`Delete the tag "${name}"? It will be removed from every entry and feed.`)) {
    return;
  }
  button.disabled = true;
  try {
    const resp = await fetch(`/tags/${encodeURIComponent(button.dataset.tag)}`, {
      method: "DELETE",
    });
    if (!resp.ok) {
      throw new Error(`${resp.status} ${resp.statusText}`);
    }
    location.href = "/tags";
  } catch (e) {
    console.error("failed to delete tag:", e);
    button.title = "Could not delete this tag";
    button.disabled = false;
  }
});

// "Copy feed URL" buttons: clicking one copies the feed's full URL to the
// clipboard, and briefly shows that it did.
document.addEventListener("click", async (event) => {
  const button = event.target.closest("button.copy-url");
  if (!button) {
    return;
  }
  const title = button.title;
  try {
    await navigator.clipboard.writeText(button.dataset.url);
    button.title = "Copied!";
  } catch (e) {
    console.error("failed to copy feed URL:", e);
    button.title = "Could not copy the URL";
  }
  setTimeout(() => {
    button.title = title;
  }, 2000);
});
