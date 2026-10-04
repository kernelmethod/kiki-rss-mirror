-- Sanitize the HTML content of new entries as they arrive, so that what
-- Kiki stores, and hands to API clients, is safe to show: it keeps only
-- the elements and attributes it allows, and drops scripts, styles,
-- embedded content, event handlers and links to unsafe URLs.
--
-- Config:
--
--   elements    A list of the elements to keep. Any other element is
--               unwrapped: its tags are removed and its content kept.
--   drop        A list of elements to remove together with their content,
--               rather than unwrapping them. Whatever this says, the
--               elements in ALWAYS_DROPPED below are always removed.
--   attributes  A list of the attributes to keep. A bare name, such as
--               "title", keeps that attribute on every element; one
--               written "element:name", such as "a:href", keeps it only on
--               that element. Every other attribute is removed, and event
--               handler attributes (onclick, onerror, ...) always are.
--   url_schemes A list of the URL schemes links and images may use, such
--               as "https". Relative URLs are always allowed. A URL
--               attribute (href, src, ...) whose scheme is not listed is
--               removed, and an image left without a source is removed.
--
-- Comments are removed. The attributes that are kept are written out
-- again from their decoded values, so a value is stored as it was checked:
-- however a URL hides its scheme behind character references, what is
-- stored is what was checked. Text is kept as it is.
--
-- The HTML is parsed with kiki.html, not with patterns. If it cannot be
-- rewritten (it needs more memory or time than plugins may use, say), the
-- entry's content is replaced with its text, escaped, so that unsanitized
-- HTML is never stored.
--
-- Only entries as they are fetched are sanitized: plugins cannot change
-- the content of entries already stored. Plugins that run after this one
-- (those whose directory names sort after "sanitize") see the sanitized
-- content, and can add markup back.

local config = ...

local function fail(message)
    error("sanitize: " .. message, 0)
end

-- Elements removed with their content whatever the config says: they run
-- scripts, apply styles, embed other documents, change how the page around
-- them behaves, or hold raw text that would be parsed as markup if they
-- were unwrapped.
local ALWAYS_DROPPED = {}
for _, name in ipairs({
    "applet", "base", "embed", "frame", "frameset", "head", "iframe",
    "link", "math", "meta", "noembed", "noframes", "noscript", "object",
    "param", "plaintext", "script", "style", "svg", "template",
    "textarea", "title", "xmp",
}) do
    ALWAYS_DROPPED[name] = true
end

-- Attributes whose values are URLs, which must be relative or use one of
-- the allowed schemes.
local URL_ATTRIBUTES = {}
for _, name in ipairs({
    "action", "background", "cite", "codebase", "data", "formaction",
    "href", "longdesc", "ping", "poster", "src", "usemap",
}) do
    URL_ATTRIBUTES[name] = true
end

-- Reads config key `key` as a list of non-empty strings, lowercased, and
-- returns them as a set.
local function string_set(key)
    local list = config[key] or {}
    if type(list) ~= "table" then
        fail("'" .. key .. "' must be a list of strings")
    end
    for k in pairs(list) do
        if math.type(k) ~= "integer" then
            fail("'" .. key .. "' must be a list of strings")
        end
    end
    local set = {}
    for i, value in ipairs(list) do
        if type(value) ~= "string" or value == "" then
            fail("'" .. key .. "' entry " .. i .. " must be a non-empty string")
        end
        set[value:lower()] = true
    end
    return set
end

local allowed_elements = string_set("elements")
local dropped_elements = string_set("drop")
local schemes = string_set("url_schemes")

-- Attributes allowed on every element, as a set, and the attributes
-- allowed on given elements, as a set per element.
local global_attributes, element_attributes = {}, {}
for value in pairs(string_set("attributes")) do
    local element, name = value:match("^([^:]+):(.+)$")
    if element ~= nil then
        element_attributes[element] = element_attributes[element] or {}
        element_attributes[element][name] = true
    else
        global_attributes[value] = true
    end
end

local function attribute_allowed(element, name)
    if name:sub(1, 2) == "on" then
        return false
    end
    if global_attributes[name] then
        return true
    end
    local allowed = element_attributes[element]
    return allowed ~= nil and allowed[name] == true
end

-- Whether `url` is relative or uses an allowed scheme. Browsers ignore
-- control characters and spaces at the start of a URL, and tabs and
-- newlines anywhere in it, so they are ignored here too: otherwise
-- " java\tscript:" would look relative.
local function url_allowed(url)
    local cleaned = url:gsub("[\t\n\r]", ""):gsub("^[%c ]+", "")
    local scheme = cleaned:match("^(%a[%w+.-]*):")
    return scheme == nil or schemes[scheme:lower()] == true
end

-- Whether every URL in `srcset`, a list of image candidates such as
-- "a.png 1x, b.png 2x", is allowed.
local function srcset_allowed(srcset)
    for candidate in srcset:gmatch("[^,]+") do
        local url = candidate:match("^%s*(%S+)")
        if url ~= nil and not url_allowed(url) then
            return false
        end
    end
    return true
end

local function value_allowed(name, value)
    if name == "srcset" then
        return srcset_allowed(value)
    end
    return not URL_ATTRIBUTES[name] or url_allowed(value)
end

local function sanitize_element(el)
    local name = el.tag_name
    if el.namespace ~= "html" or ALWAYS_DROPPED[name] or dropped_elements[name] then
        el:remove()
        return
    end
    if not allowed_elements[name] then
        el:remove_and_keep_content()
        return
    end

    -- Remove every attribute, then set the ones that are allowed again
    -- from their decoded values.
    local attributes = el:attributes()
    for _, attribute in ipairs(attributes) do
        el:remove_attribute(attribute.name)
    end
    for _, attribute in ipairs(attributes) do
        if attribute_allowed(name, attribute.name)
            and value_allowed(attribute.name, attribute.value) then
            el:set_attribute(attribute.name, attribute.value)
        end
    end

    if name == "img" and not el:has_attribute("src") then
        el:remove()
    end
end

local handlers = {
    elements = { { "*", sanitize_element } },
    comments = function(comment) comment:remove() end,
}

-- The text of `html`, escaped: what an entry's content becomes when it
-- cannot be sanitized.
local function as_text(html)
    return kiki.html.escape(kiki.html.unescape((html:gsub("<[^>]*>", ""))))
end

kiki.on("entry.ingest", function(entry)
    if entry.content ~= nil then
        local ok, result = pcall(kiki.html.rewrite, entry.content, handlers)
        if ok then
            entry.content = result
        else
            kiki.log("warn", "sanitize: unable to sanitize the content of entry "
                .. entry.guid .. ", keeping only its text: " .. tostring(result))
            entry.content = as_text(entry.content)
        end
    end
    return entry
end)
