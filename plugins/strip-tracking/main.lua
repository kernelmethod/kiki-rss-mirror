-- Strip tracking parameters, such as utm_source or fbclid, from the URLs
-- in new entries.
--
-- Config:
--
--   params   A list of the query parameters to remove. A name ending in
--            `*` removes every parameter whose name starts with the rest
--            of it, so "utm_*" removes utm_source, utm_medium and so on.
--            Names are matched ignoring case, and as they are written in
--            the URL (percent-encoded names are not decoded).
--   content  Whether to also clean the links in each entry's content: the
--            values of its href and src attributes. Defaults to true.
--
-- The parameters are removed from an entry's query string, and from its
-- fragment when that is written like one (`#xtor=RSS-1`). Every other part
-- of a URL is left as it was, and a URL with nothing to remove is not
-- touched at all. A query or fragment left empty is dropped, along with
-- its `?` or `#`. In content, `&amp;` is understood as a separator too.
--
-- Only entries as they are fetched are cleaned: plugins cannot change the
-- URL or content of entries already stored, so adding a parameter to the
-- list does not clean the entries downloaded before.

local config = ...

local function fail(message)
    error("strip-tracking: " .. message, 0)
end

-- The names in `params`, lowercased: exact names as a set, and the
-- prefixes given by names ending in `*` as a list.
local names, prefixes = {}, {}

local params = config.params or {}
if type(params) ~= "table" then
    fail("'params' must be a list of parameter names")
end
for i, param in ipairs(params) do
    if type(param) ~= "string" or param == "" or param == "*" then
        fail(string.format(
            "params[%d] must be a parameter name, or a prefix followed by '*'", i))
    end
    param = param:lower()
    if param:sub(-1) == "*" then
        table.insert(prefixes, param:sub(1, -2))
    else
        names[param] = true
    end
end

local function is_tracking(name)
    name = name:lower()
    if names[name] then
        return true
    end
    for _, prefix in ipairs(prefixes) do
        if name:sub(1, #prefix) == prefix then
            return true
        end
    end
    return false
end

-- Removes the tracking parameters from `s`, a query string or a fragment,
-- without its `?` or `#`. Returns the string left, or nil if nothing is
-- left, and whether anything was removed.
--
-- Parameters are separated by `&`, or by `&amp;` as in HTML. Each one kept
-- keeps the separator that came before it.
local function strip_params(s)
    local parts = {}
    local removed = false
    local pos, sep = 1, ""
    while true do
        local i = s:find("&", pos, true)
        local part = i and s:sub(pos, i - 1) or s:sub(pos)
        if part ~= "" and is_tracking(part:match("^[^=]*")) then
            removed = true
        else
            table.insert(parts, { sep = sep, text = part })
        end
        if not i then
            break
        end
        sep = s:sub(i, i + 4) == "&amp;" and "&amp;" or "&"
        pos = i + #sep
    end
    if not removed then
        return s, false
    end

    local out = {}
    for _, part in ipairs(parts) do
        -- Empty parts (as in `a=1&&b=2`) would leave stray separators.
        if part.text ~= "" then
            if #out > 0 then
                table.insert(out, part.sep)
            end
            table.insert(out, part.text)
        end
    end
    if #out == 0 then
        return nil, true
    end
    return table.concat(out), true
end

-- Returns `url` with its tracking parameters removed.
local function clean_url(url)
    local base, fragment = url:match("^([^#]*)#(.*)$")
    if not base then
        base = url
    end
    local path, query = base:match("^([^?]*)%?(.*)$")
    if not path then
        path = base
    end

    local query_changed, fragment_changed = false, false
    if query then
        query, query_changed = strip_params(query)
    end
    if fragment then
        fragment, fragment_changed = strip_params(fragment)
    end
    if not query_changed and not fragment_changed then
        return url
    end
    return path
        .. (query and ("?" .. query) or "")
        .. (fragment and ("#" .. fragment) or "")
end

-- Attribute names are matched only after whitespace, so that `data-src`,
-- say, is left alone. Lua patterns have no case-insensitive matching.
local ATTRIBUTES = { "[Hh][Rr][Ee][Ff]", "[Ss][Rr][Cc]" }

-- Returns `html` with the tracking parameters removed from the URLs in its
-- href and src attributes.
local function clean_content(html)
    for _, attribute in ipairs(ATTRIBUTES) do
        -- Quoted values: `%2` matches the same quote the value opened with.
        html = html:gsub("(%s" .. attribute .. "%s*=%s*)([\"'])(.-)%2",
            function(before, quote, value)
                return before .. quote .. clean_url(value) .. quote
            end)
        -- Unquoted values.
        html = html:gsub("(%s" .. attribute .. "%s*=%s*)([^%s\"'`=<>][^%s>]*)",
            function(before, value)
                return before .. clean_url(value)
            end)
    end
    return html
end

kiki.on("entry.ingest", function(entry)
    if entry.url ~= nil then
        entry.url = clean_url(entry.url)
    end
    if entry.content ~= nil and config.content ~= false then
        entry.content = clean_content(entry.content)
    end
    return entry
end)
