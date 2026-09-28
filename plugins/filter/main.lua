-- Hide entries whose fields match, or fail to match, regular expressions,
-- and tag the entries that match others.
--
-- Config:
--
--   exclude  A list of rules. An entry matching any of them is hidden.
--   include  A list of rules. An entry whose feed has include rules, and
--            that matches none of them, is hidden.
--   tag      A list of rules, each with a `tag` too. An entry matching a
--            rule is tagged with its tag. Hidden entries are not tagged.
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
--            one of the entry's). Defaults to { "title", "content" }. A
--            single name is accepted in place of a list, but the settings
--            in manifest.toml, which the web UI and config API go by, only
--            allow lists.
--   feeds    Optional list of the feeds the rule applies to, each given
--            by its id or by the URL it is fetched from. Without it, the
--            rule applies to every feed.
--   tag      For tag rules only: the tag to add, a user tag or a system
--            tag such as system:saved.
--
-- Hidden entries are tagged system:hidden. The filter never unhides an
-- entry or removes a tag, so loosening a rule leaves the entries it hid
-- hidden, and the entries it tagged tagged.
--
-- Since the tags a plugin returns for an entry replace its user tags (see
-- the scripting documentation), a tag rule that matches an entry fetched
-- again replaces the user tags it was given since with the rule's tag.

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

    local fields = rule.fields or DEFAULT_FIELDS
    if type(fields) == "string" then
        fields = { fields }
    end
    if type(fields) ~= "table" or #fields == 0 then
        fail(where, "'fields' must be a field name or a non-empty list of them")
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
        local where = string.format("%s[%d]", name, i)
        local compiled = compile_rule(where, rule)
        if name == "tag" then
            if type(rule.tag) ~= "string" or rule.tag == "" then
                fail(where, "'tag' must be a tag name")
            end
            compiled.tag = rule.tag
        end
        table.insert(rules, compiled)
    end
    return rules
end

-- Compiled here, so that a bad rule fails when the plugin loads rather than
-- on every entry.
local exclude = compile_rules("exclude")
local include = compile_rules("include")
local tag_rules = compile_rules("tag")

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

-- Adds `name` to the entry's tags. A stored entry, handed to a scan, is
-- tagged directly, since a scan only applies the system tags its handler
-- adds.
local function add_tag(entry, name)
    if entry.id ~= nil then
        kiki.entries.tag(entry.id, name)
    elseif not has_tag(entry, name) then
        -- An earlier plugin may have added the tag already. (Entries
        -- passed to a scan always arrive with `tags` empty.)
        table.insert(entry.tags, name)
    end
end

local function filter(entry)
    local reason = reason_to_hide(entry)
    if reason then
        kiki.log("debug", string.format("filter: hiding %q: %s", entry.guid, reason))
        if not has_tag(entry, HIDDEN) then
            table.insert(entry.tags, HIDDEN)
        end
        return entry
    end
    for _, rule in ipairs(tag_rules) do
        if applies(rule, entry) and matches(rule, entry) then
            kiki.log("debug", string.format(
                "filter: tagging %q %s: %s matched", entry.guid, rule.tag, rule.where))
            add_tag(entry, rule.tag)
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
    -- `tag` is only recorded when there are tag rules, so that rules
    -- recorded before tag rules existed still compare equal.
    local tag = config.tag
    if tag ~= nil and #tag == 0 then
        tag = nil
    end
    local rules = { exclude = config.exclude or {}, include = config.include or {}, tag = tag }
    if deep_equal(kiki.store.get("rules"), rules) then
        return
    end
    if #exclude == 0 and #include == 0 and #tag_rules == 0 then
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
