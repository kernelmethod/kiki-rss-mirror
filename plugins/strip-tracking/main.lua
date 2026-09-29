-- Strip tracking parameters, such as utm_source or fbclid, from the URLs
-- in new entries, and tracking pixels from their content, and keep Kiki
-- from downloading any images at all for the feeds that ask for it.
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
--   pixels   Whether to remove tracking pixels from each entry's content:
--            images declared no bigger than 1x1 (both their width and
--            height attributes are 0 or 1), and images from the trackers
--            in `trackers`. Defaults to true.
--   trackers A list of the image sources whose images are always removed,
--            when `pixels` is true. Each is a host name, which may start
--            with `*.` to match the domain and all of its subdomains, and
--            may be followed by the start of a path: "medium.com/_/stat"
--            removes images from https://medium.com/_/stat?event=... but
--            not other images from medium.com. Host names are matched
--            ignoring case, paths as they are written.
--   skip_assets
--            A list of feeds, each given by its id or by the URL it is
--            fetched from, whose entries' images and enclosures are never
--            downloaded into Kiki's asset cache, so that the sites they
--            are served from never hear from Kiki. The entries are stored
--            as they are; only the downloads are skipped. Empty by default.
--
-- The parameters are removed from an entry's query string, and from its
-- fragment when that is written like one (`#xtor=RSS-1`). Every other part
-- of a URL is left as it was, and a URL with nothing to remove is not
-- touched at all. A query or fragment left empty is dropped, along with
-- its `?` or `#`. In content, `&amp;` is understood as a separator too.
--
-- Removing a tracking pixel removes its whole <img> element, so the image
-- is neither shown nor downloaded into Kiki's asset cache.
--
-- Only entries as they are fetched are cleaned: plugins cannot change the
-- URL or content of entries already stored, so adding a parameter or a
-- tracker to the list does not clean the entries downloaded before.

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

-- The entries in `trackers`, each compiled to a table holding the host
-- name to match (`host`, or `domain` for a `*.` name) and the start of the
-- path to match (`path`, possibly empty).
local trackers = {}

local tracker_list = config.trackers or {}
if type(tracker_list) ~= "table" then
    fail("'trackers' must be a list of host names")
end
for i, tracker in ipairs(tracker_list) do
    local host, path
    if type(tracker) == "string" then
        host, path = tracker:match("^([%w._-]+)(.*)$")
        if not host then
            host, path = tracker:match("^%*%.([%w._-]+)(.*)$")
            host = host and "*." .. host
        end
    end
    if not host or not (path == "" or path:match("^/%S*$")) then
        fail(string.format(
            "trackers[%d] must be a host name, optionally starting with '*.' "
            .. "and followed by a path", i))
    end
    host = host:lower()
    if host:sub(1, 2) == "*." then
        table.insert(trackers, { domain = host:sub(3), path = path })
    else
        table.insert(trackers, { host = host, path = path })
    end
end

-- Whether `src`, an image's source, is from one of the `trackers`.
local function is_tracker(src)
    local host, rest = src:match("^%s*[%a][%w+.-]*://([^/?#]*)(.*)$")
    if not host then
        host, rest = src:match("^%s*//([^/?#]*)(.*)$")
    end
    if not host then
        return false
    end
    -- Drop any credentials and port, and the trailing dot of a
    -- fully-qualified name.
    host = host:gsub("^.*@", ""):gsub(":%d*$", ""):gsub("%.$", ""):lower()
    for _, tracker in ipairs(trackers) do
        local host_matches
        if tracker.domain then
            host_matches = host == tracker.domain
                or host:sub(-(#tracker.domain + 1)) == "." .. tracker.domain
        else
            host_matches = host == tracker.host
        end
        if host_matches and rest:sub(1, #tracker.path) == tracker.path then
            return true
        end
    end
    return false
end

-- Whether `value`, a width or height attribute, is at most one pixel.
local function is_tiny(value)
    local n = value and tonumber(value:match("^%s*([%d.]+)%s*[Pp]?[Xx]?%s*$"))
    return n ~= nil and n <= 1
end

-- Whether an image with the attributes `attrs` is a tracking pixel.
local function is_pixel(attrs)
    if is_tiny(attrs.width) and is_tiny(attrs.height) then
        return true
    end
    return attrs.src ~= nil and is_tracker(attrs.src)
end

-- Reads the attributes of the tag in `html` whose name ends just before
-- `pos`. Returns them, as a table of lowercased names to values (the first
-- of any given more than once), and the position of the `>` closing the
-- tag; or nothing, if the tag is not closed.
local function read_tag(html, pos)
    local attrs = {}
    while true do
        pos = html:find("[^%s/]", pos)
        if not pos then
            return
        end
        if html:sub(pos, pos) == ">" then
            return attrs, pos
        end
        local name_end = (html:find("[%s/>=]", pos + 1) or #html + 1) - 1
        local name = html:sub(pos, name_end):lower()
        local value = ""
        pos = html:find("%S", name_end + 1)
        if not pos then
            return
        end
        if html:sub(pos, pos) == "=" then
            pos = html:find("%S", pos + 1)
            if not pos then
                return
            end
            local quote = html:sub(pos, pos)
            if quote == '"' or quote == "'" then
                local close = html:find(quote, pos + 1, true)
                if not close then
                    return
                end
                value = html:sub(pos + 1, close - 1)
                pos = close + 1
            else
                local value_end = (html:find("[%s>]", pos) or #html + 1) - 1
                value = html:sub(pos, value_end)
                pos = value_end + 1
            end
        end
        if attrs[name] == nil then
            attrs[name] = value
        end
    end
end

-- Returns `html` with its tracking pixels' <img> elements removed.
local function remove_pixels(html)
    local out = {}
    local pos = 1
    while true do
        local start, name_end = html:find("<[Ii][Mm][Gg]", pos)
        if not start then
            break
        end
        local attrs, close
        if html:sub(name_end + 1, name_end + 1):match("[%s/>]") then
            attrs, close = read_tag(html, name_end + 1)
        end
        if attrs and is_pixel(attrs) then
            table.insert(out, html:sub(pos, start - 1))
            pos = close + 1
        else
            -- Not an <img> tag (say, <imgx>), one to keep, or one left
            -- unclosed: carry on after its name.
            table.insert(out, html:sub(pos, name_end))
            pos = name_end + 1
        end
    end
    table.insert(out, html:sub(pos))
    return table.concat(out)
end

-- The feeds in `skip_assets`: a set of feed ids, and a set of feed URLs.
local skip_ids, skip_urls = {}, {}

local skip_list = config.skip_assets or {}
if type(skip_list) ~= "table" then
    fail("'skip_assets' must be a list of feed ids or URLs")
end
for i, feed in ipairs(skip_list) do
    if type(feed) == "string" and feed ~= "" then
        skip_urls[feed] = true
    else
        local id = type(feed) == "number" and math.tointeger(feed)
        if not id then
            fail(string.format("skip_assets[%d] must be a feed id or URL", i))
        end
        skip_ids[id] = true
    end
end

-- Whether feed `feed_id` is in `skip_assets`, by id or by URL, looked up
-- once per feed.
local skips = {}

local function skips_assets(feed_id)
    if skip_ids[feed_id] then
        return true
    end
    if next(skip_urls) == nil then
        return false
    end
    local skip = skips[feed_id]
    if skip == nil then
        local feed = kiki.feeds.get(feed_id)
        skip = feed ~= nil and feed.url ~= nil and skip_urls[feed.url] == true
        skips[feed_id] = skip
    end
    return skip
end

-- A feed's URL changes when it is permanently redirected, and a removed
-- feed's id may be given to a new feed.
kiki.on("fetch.success", function(fetch)
    skips[fetch.feed_id] = nil
end)
kiki.on("feed.removed", function(feed)
    skips[feed.id] = nil
end)

kiki.on("entry.ingest", function(entry)
    if entry.url ~= nil then
        entry.url = clean_url(entry.url)
    end
    if entry.content ~= nil and config.pixels ~= false then
        entry.content = remove_pixels(entry.content)
    end
    if entry.content ~= nil and config.content ~= false then
        entry.content = clean_content(entry.content)
    end
    if skips_assets(entry.feed_id) then
        entry.cache_assets = false
    end
    return entry
end)
