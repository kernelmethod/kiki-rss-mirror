-- Hide entries whose fields match, or fail to match, regular expressions.
--
-- Config:
--
--   exclude  A list of rules. An entry matching any of them is hidden.
--   include  A list of rules. An entry whose feed has include rules, and
--            that matches none of them, is hidden.
--   rescan   Whether to apply the rules to the entries already stored
--            when they change. Defaults to true.
--
-- A rule is a table with:
--
--   pattern  The regular expression, in the syntax of kiki.regex.
--   flags    Optional kiki.regex flags, such as "i" for case-insensitive.
--   fields   The entry fields to match: a list of names, from title, url,
--            content, authors, categories and guid. A rule matches if the
--            pattern matches any of them (for authors and categories, any
--            one of the entry's). Missing or empty, it is { "title",
--            "content" }. A single name is accepted in place of a list,
--            but the settings in manifest.toml, which the web UI and
--            config API go by, only allow lists.
--   feeds    Optional list of the feeds the rule applies to, each given
--            by its id or by the URL it is fetched from. Without it, the
--            rule applies to every feed.
--
-- Hidden entries are tagged system:hidden. The filter never unhides an
-- entry, so loosening a rule leaves the entries it hid hidden.
--
-- The filter adds no other tags: the auto-tag plugin does that. Versions
-- before 3.0.0 took `tag` rules too; they are now ignored, with a warning.

local config = ...

local HIDDEN = "system:hidden"

-- The URL of each feed looked up so far, by feed id; false for a feed with
-- no URL, or that does not exist.
local feed_urls = {}

local function feed_url(feed_id)
    local url = feed_urls[feed_id]
    if url == nil then
        local feed = kiki.feeds.get(feed_id)
        url = (feed and feed.url) or false
        feed_urls[feed_id] = url
    end
    return url or nil
end

-- The fields a rule can match, and how to read each as a list of strings.
local FIELDS = {
    title = function(e) return { e.title } end,
    url = function(e) return { e.url } end,
    content = function(e) return { e.content } end,
    guid = function(e) return { e.guid } end,
    authors = function(e) return e.authors or {} end,
    categories = function(e) return e.categories or {} end,
}

local DEFAULT_FIELDS = { "title", "content" }

local function fail(where, message)
    error(string.format("filter: %s: %s", where, message), 0)
end

local function compile_rule(where, rule)
    if type(rule) ~= "table" then
        fail(where, "a rule must be a table")
    end
    if type(rule.pattern) ~= "string" then
        fail(where, "'pattern' must be a string")
    end

    local fields = rule.fields
    if fields == nil or (type(fields) == "table" and #fields == 0) then
        fields = DEFAULT_FIELDS
    elseif type(fields) == "string" then
        fields = { fields }
    end
    if type(fields) ~= "table" or #fields == 0 then
        fail(where, "'fields' must be a field name or a list of them")
    end
    local readers = {}
    for _, name in ipairs(fields) do
        local reader = FIELDS[name]
        if not reader then
            fail(where, string.format(
                "unknown field %q; expected title, url, content, authors, categories or guid",
                tostring(name)))
        end
        table.insert(readers, reader)
    end

    local ok, re = pcall(kiki.regex, rule.pattern, rule.flags)
    if not ok then
        fail(where, tostring(re))
    end

    -- Feed ids and feed URLs, each a set.
    local feeds, urls = nil, nil
    if rule.feeds ~= nil then
        if type(rule.feeds) ~= "table" then
            fail(where, "'feeds' must be a list of feed ids or URLs")
        end
        feeds, urls = {}, {}
        for _, feed in ipairs(rule.feeds) do
            if type(feed) == "string" then
                urls[feed] = true
            else
                local n = math.tointeger(feed)
                if not n then
                    fail(where, "'feeds' must be a list of feed ids or URLs")
                end
                feeds[n] = true
            end
        end
        if next(urls) == nil then
            urls = nil
        end
    end

    return { re = re, readers = readers, feeds = feeds, urls = urls, where = where }
end

local function compile_rules(name)
    local rules = {}
    local list = config[name] or {}
    if type(list) ~= "table" then
        fail(name, "must be a list of rules")
    end
    for i, rule in ipairs(list) do
        table.insert(rules, compile_rule(string.format("%s[%d]", name, i), rule))
    end
    return rules
end

-- Compiled here, so that a bad rule fails when the plugin loads rather than
-- on every entry.
local exclude = compile_rules("exclude")
local include = compile_rules("include")

if type(config.tag) == "table" and #config.tag > 0 then
    kiki.log("warn", "filter: ignoring the 'tag' rules; the filter no longer tags "
        .. "entries, so move them to the auto-tag plugin's rules")
end

local function applies(rule, entry)
    if rule.feeds == nil or rule.feeds[entry.feed_id] then
        return true
    end
    if rule.urls then
        local url = feed_url(entry.feed_id)
        return url ~= nil and rule.urls[url] == true
    end
    return false
end

local function matches(rule, entry)
    for _, read in ipairs(rule.readers) do
        for _, value in ipairs(read(entry)) do
            if type(value) == "string" and rule.re:is_match(value) then
                return true
            end
        end
    end
    return false
end

-- Returns why `entry` should be hidden, or nil if it should not be.
local function reason_to_hide(entry)
    for _, rule in ipairs(exclude) do
        if applies(rule, entry) and matches(rule, entry) then
            return rule.where .. " matched"
        end
    end
    local any_include = false
    for _, rule in ipairs(include) do
        if applies(rule, entry) then
            if matches(rule, entry) then
                return nil
            end
            any_include = true
        end
    end
    if any_include then
        return "no include rule matched"
    end
    return nil
end

local function has_tag(entry, name)
    for _, tag in ipairs(entry.tags) do
        if tag == name then
            return true
        end
    end
    return false
end

local function filter(entry)
    local reason = reason_to_hide(entry)
    if reason then
        kiki.log("debug", string.format("filter: hiding %q: %s", entry.guid, reason))
        if not has_tag(entry, HIDDEN) then
            table.insert(entry.tags, HIDDEN)
        end
    end
    return entry
end

kiki.on("entry.ingest", filter)

-- A feed's id may be given to a new feed once it is removed.
kiki.on("feed.removed", function(feed)
    feed_urls[feed.id] = nil
end)

local function deep_equal(a, b)
    if type(a) ~= "table" or type(b) ~= "table" then
        return a == b
    end
    for k, v in pairs(a) do
        if not deep_equal(v, b[k]) then
            return false
        end
    end
    for k in pairs(b) do
        if a[k] == nil then
            return false
        end
    end
    return true
end

-- When the rules change, apply them to the entries already stored. The
-- rules last applied are kept in the plugin's store, so that restarting
-- the server, or reloading plugins for some other reason, does not rescan:
-- that would hide again any entry the user unhid.
--
-- The rules are only recorded as applied once the scan has gone through
-- every entry. A scan cut short, by a reload or the server stopping, runs
-- again from the start on the next load.
kiki.on("plugin.load", function()
    if config.rescan == false then
        return
    end
    local rules = { exclude = config.exclude or {}, include = config.include or {} }
    -- Versions before 3.0.0 recorded their tag rules too. Those no longer
    -- apply, so dropping them should not rescan.
    local recorded = kiki.store.get("rules")
    if type(recorded) == "table" then
        recorded.tag = nil
    end
    if deep_equal(recorded, rules) then
        return
    end
    if #exclude == 0 and #include == 0 then
        kiki.store.set("rules", rules)
        return
    end
    kiki.log("info", "filter: rules changed; applying them to stored entries")
    kiki.entries.scan(filter, function(summary)
        kiki.log("info", string.format(
            "filter: applied the rules to %d stored entries, hiding %d",
            summary.scanned, summary.updated))
        kiki.store.set("rules", rules)
    end)
end)
