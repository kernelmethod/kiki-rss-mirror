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
