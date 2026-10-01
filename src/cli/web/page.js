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
    const resp = await fetch(`/entries/${button.dataset.entry}/system-tags/saved`, {
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
// clipboard, and briefly shows a "Copied!" popup above the button.
document.addEventListener("click", async (event) => {
  const button = event.target.closest("button.copy-url");
  if (!button) {
    return;
  }
  clearTimeout(button.copyStatusTimer);
  try {
    await navigator.clipboard.writeText(button.dataset.url);
    button.dataset.status = "Copied!";
    delete button.dataset.statusError;
  } catch (e) {
    console.error("failed to copy feed URL:", e);
    button.dataset.status = "Could not copy the URL";
    button.dataset.statusError = "";
  }
  button.copyStatusTimer = setTimeout(() => {
    delete button.dataset.status;
    delete button.dataset.statusError;
  }, 2000);
});

// Swiping an unread entry left marks it as read: the row follows the finger,
// uncovering a "Read" panel, and once dragged far enough it slides away,
// collapses, and is tagged `system:read`. A popup at the bottom of the page
// offers to undo it. Only touches swipe; a mouse drag does nothing.
const SWIPE_START = 10; // px moved left before the row starts to follow
const SWIPE_COMMIT = 0.35; // fraction of the row's width that marks it read
const SWIPE_FLICK = 0.5; // px per ms: a flick this fast marks it read too
const UNDO_TIMEOUT = 5000;

let swipe = null;
let lastSwipeEnd = 0;

function reducedMotion() {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

document.addEventListener("pointerdown", (event) => {
  const row = event.target.closest("li.swipe-read");
  if (!row || event.pointerType !== "touch" || !event.isPrimary || swipe || row.swipeAnimations) {
    return;
  }
  swipe = { row, id: event.pointerId, x: event.clientX, y: event.clientY, claimed: false, dragging: false };
});

document.addEventListener("pointermove", (event) => {
  if (!swipe || event.pointerId !== swipe.id) {
    return;
  }
  const dx = event.clientX - swipe.x;
  const dy = event.clientY - swipe.y;
  if (!swipe.claimed) {
    if (dx === 0 && dy === 0) {
      return;
    }
    // Decide between swipe and scroll on the very first move: browsers only
    // let a page keep a touch from scrolling if it says so then (see the
    // `touchmove` listener below). A move that is mostly vertical is a
    // scroll, and one to the right is not a swipe we act on; either way,
    // leave it to the browser.
    if (dx >= 0 || Math.abs(dy) >= Math.abs(dx)) {
      swipe = null;
      return;
    }
    swipe.claimed = true;
  }
  if (!swipe.dragging) {
    // Until the finger has gone a little way, this may still be a tap.
    if (-dx < SWIPE_START) {
      return;
    }
    swipe.dragging = true;
    swipe.row.setPointerCapture(event.pointerId);
    swipe.row.classList.add("swiping");
  }
  const offset = Math.min(0, dx);
  if (swipe.time !== undefined) {
    swipe.speed = (offset - swipe.offset) / Math.max(1, event.timeStamp - swipe.time);
  }
  swipe.offset = offset;
  swipe.time = event.timeStamp;
  swipe.row.style.transform = `translateX(${offset}px)`;
});

// Keep the browser from scrolling during a swipe. `touch-action: pan-y`
// alone is not enough: some browsers drop the declaration (WebKit does not
// know `pinch-zoom`), and Firefox on Android starts scrolling anyway once the
// finger drifts far enough up or down. Either way the swipe is cancelled and
// the row snaps back. Touch events come after the pointer events for the
// same move, so `claimed` is already set on the first move of a swipe.
document.addEventListener(
  "touchmove",
  (event) => {
    if (swipe?.claimed && event.cancelable) {
      event.preventDefault();
    }
  },
  { passive: false },
);

function endSwipe(event) {
  if (!swipe || event.pointerId !== swipe.id) {
    return;
  }
  const { row, dragging, offset = 0, speed = 0 } = swipe;
  swipe = null;
  if (!dragging) {
    return;
  }
  lastSwipeEnd = Date.now();
  // A row dragged far enough is marked read even if the browser cancelled
  // the gesture before the finger came up.
  const commit =
    -offset > row.offsetWidth * SWIPE_COMMIT ||
    (event.type === "pointerup" && -speed > SWIPE_FLICK && -offset > SWIPE_START * 3);
  if (commit) {
    markRead(row, offset);
  } else {
    snapBack(row, offset);
  }
}
document.addEventListener("pointerup", endSwipe);
document.addEventListener("pointercancel", endSwipe);

// A swipe that ends over the entry's link or save button must not also
// follow or press it.
document.addEventListener(
  "click",
  (event) => {
    if (Date.now() - lastSwipeEnd < 400 && event.target.closest("li.swipe-read")) {
      event.preventDefault();
      event.stopImmediatePropagation();
    }
  },
  true,
);

async function snapBack(row, offset) {
  row.style.transform = "";
  const animation = row.animate(
    [{ transform: `translateX(${offset}px)` }, { transform: "translateX(0)" }],
    { duration: reducedMotion() ? 0 : 150, easing: "ease-out" },
  );
  await animation.finished.catch(() => {});
  row.classList.remove("swiping");
}

// Slide `row` the rest of the way out and collapse the gap it leaves, while
// telling the server it is read. If that fails, the row comes back.
async function markRead(row, offset) {
  const duration = reducedMotion() ? 0 : 150;
  const style = getComputedStyle(row);
  row.style.transform = "";
  const slide = row.animate(
    [{ transform: `translateX(${offset}px)` }, { transform: "translateX(-100%)" }],
    { duration, easing: "ease-out", fill: "forwards" },
  );
  row.swipeAnimations = [slide];
  const request = setRead(row, true);
  adjustUnreadCount(-1);
  showUndo(row, request);

  await slide.finished.catch(() => {});
  if (!row.swipeAnimations) {
    return; // Undone already.
  }
  const collapse = row.animate(
    [
      {
        height: `${row.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom)}px`,
        paddingTop: style.paddingTop,
        paddingBottom: style.paddingBottom,
        borderBottomWidth: style.borderBottomWidth,
      },
      { height: "0px", paddingTop: "0px", paddingBottom: "0px", borderBottomWidth: "0px" },
    ],
    { duration, easing: "ease-in", fill: "forwards" },
  );
  row.swipeAnimations.push(collapse);
  await collapse.finished.catch(() => {});
  if (!row.swipeAnimations) {
    return;
  }
  row.hidden = true;

  // Undoing puts the row back itself, whether or not this worked.
  if (!(await request) && row.swipeAnimations) {
    restoreRow(row);
    adjustUnreadCount(1);
    row.title = "Could not mark this entry as read";
    showToast("Could not mark the entry as read.");
  }
}

// Put back a row that was swiped away.
function restoreRow(row) {
  row.swipeAnimations?.forEach((animation) => animation.cancel());
  row.swipeAnimations = null;
  row.hidden = false;
  row.classList.remove("swiping");
}

// Add (`read`) or remove the entry's `system:read` tag; resolves to whether
// that worked.
async function setRead(row, read) {
  try {
    const resp = await fetch(`/entries/${row.dataset.entry}/system-tags/read`, {
      method: read ? "PUT" : "DELETE",
    });
    if (!resp.ok) {
      throw new Error(`${resp.status} ${resp.statusText}`);
    }
    return true;
  } catch (e) {
    console.error(`failed to mark entry as ${read ? "read" : "unread"}:`, e);
    return false;
  }
}

// Keep the "N unread entries" count above the list in step with swipes.
function adjustUnreadCount(delta) {
  const count = document.querySelector(".list-header .count");
  const n = parseInt(count?.textContent, 10);
  if (Number.isNaN(n)) {
    return;
  }
  const total = Math.max(0, n + delta);
  count.textContent = `${total} unread ${total === 1 ? "entry" : "entries"}`;
}

// The popup at the bottom of the page. Only the latest swipe can be undone.
function showToast(message, undo) {
  let toast = document.querySelector(".toast");
  if (!toast) {
    toast = document.createElement("div");
    toast.className = "toast";
    toast.setAttribute("role", "status");
    document.body.append(toast);
  }
  clearTimeout(toast.timer);
  toast.replaceChildren(message);
  if (undo) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Undo";
    button.addEventListener("click", () => {
      toast.hidden = true;
      undo();
    });
    toast.append(" ", button);
  }
  toast.hidden = false;
  toast.timer = setTimeout(() => {
    toast.hidden = true;
  }, UNDO_TIMEOUT);
}

function showUndo(row, request) {
  showToast("Marked as read.", async () => {
    restoreRow(row);
    adjustUnreadCount(1);
    // Wait for the entry to be marked read, so that the two requests
    // cannot pass each other; if it never was, there is nothing to undo.
    if ((await request) && !(await setRead(row, false))) {
      row.hidden = true;
      adjustUnreadCount(-1);
      showToast("Could not mark the entry as unread.");
    }
  });
}
