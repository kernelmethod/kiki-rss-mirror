// Adds a link from the guide's top bar back to the landing page, which
// sits one directory above the guide (see flake.nix).
(function () {
    const buttons = document.querySelector("#mdbook-menu-bar .right-buttons");
    if (!buttons) {
        return;
    }
    const link = document.createElement("a");
    link.href = path_to_root + "../";
    link.title = "Kiki home";
    link.setAttribute("aria-label", "Kiki home");
    // Styled like mdBook's own buttons, which wrap their icons in .fa-svg.
    link.innerHTML =
        '<span class="fa-svg"><svg viewBox="0 0 24 24" aria-hidden="true">' +
        '<path d="M12 3 2 11.5h3V21h5.5v-6h3v6H19v-9.5h3z"/></svg></span>';
    buttons.prepend(link);
})();
