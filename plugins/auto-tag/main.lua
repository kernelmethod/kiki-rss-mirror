-- Tag entries automatically: those whose fields match a regular
-- expression, those from given feeds, or both.
--
-- Config:
--
--   rules    A list of rules. An entry matching a rule is tagged with its
--            tag; an entry matching several rules gets each of their tags.
--   rescan   Whether to apply the rules to the entries already stored
--            when they change. Defaults to true.
--
-- A rule is a table with:
--
--   tag      The tag to add: a user tag, or one of the system tags
--            system:read, system:saved and system:hidden.
--   pattern  Optional regular expression, in the syntax of kiki.regex.
--            Without it, the rule matches every entry from its feeds.
--   flags    Optional kiki.regex flags, such as "i" for case-insensitive.
--   fields   The entry fields to match the pattern against: a list of
--            names, from title, url, content, authors, categories and
--            guid. The pattern matches if it matches any of them (for
--            authors and categories, any one of the entry's). Missing or
--            empty, it is { "title", "content" }. A single name is
--            accepted in place of a list, but the settings in
--            manifest.toml, which the web UI and config API go by, only
--            allow lists.
--   feeds    Optional list of the feeds the rule applies to, each given
--            by its id or by the URL it is fetched from. Missing or
--            empty, the rule applies to every feed.
--
-- A rule needs a pattern, feeds, or both: one with neither would tag every
-- entry, and fails to load.
--
-- The plugin never removes a tag, so loosening or deleting a rule leaves
-- the entries it tagged tagged.
--
-- Since the tags a plugin returns for an entry replace its user tags (see
-- the scripting documentation), a rule that matches an entry fetched again
-- replaces the user tags it was given since with the rule's tag.

local config = ...

-- The system tags a rule may add. Any other name starting with `system:`
-- is reserved.
local SYSTEM_TAGS = {
    ["system:read"] = true,
    ["system:saved"] = true,
    ["system:hidden"] = true,
}

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
    error(string.format("auto-tag: %s: %s", where, message), 0)
end

local function compile_tag(where, tag)
    if type(tag) ~= "string" or tag == "" then
        fail(where, "'tag' must be a tag name")
    end
    if tag:sub(1, #"system:") == "system:" and not SYSTEM_TAGS[tag] then
        fail(where, string.format(
            "unknown system tag %q; expected system:read, system:saved or system:hidden", tag))
    end
    return tag
end

-- Returns the rule's regex and the readers for its fields, or nil if the
-- rule has no pattern.
local function compile_pattern(where, rule)
    local pattern = rule.pattern
    if pattern == nil or pattern == "" then
        return nil, nil
    end
    if type(pattern) ~= "string" then
        fail(where, "'pattern' must be a string")
    end

    local fields = rule.fields
    if fields == nil or (type(fields) == "table" and #fields == 0) then
        fields = DEFAULT_FIELDS
    elseif type(fields) == "string" then
        fields = { fields }
    end
    if type(fields) ~= "table" then
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

    local ok, re = pcall(kiki.regex, pattern, rule.flags)
    if not ok then
        fail(where, tostring(re))
    end
    return re, readers
end

-- Returns the rule's feed ids and feed URLs, each a set, or nil for a rule
-- that applies to every feed.
local function compile_feeds(where, list)
    if list == nil then
        return nil, nil
    end
    if type(list) ~= "table" then
        fail(where, "'feeds' must be a list of feed ids or URLs")
    end
    if #list == 0 then
        return nil, nil
    end
    local feeds, urls = {}, {}
    for _, feed in ipairs(list) do
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
    return feeds, urls
end

local function compile_rule(where, rule)
    if type(rule) ~= "table" then
        fail(where, "a rule must be a table")
    end
    local tag = compile_tag(where, rule.tag)
    local re, readers = compile_pattern(where, rule)
    local feeds, urls = compile_feeds(where, rule.feeds)
    if re == nil and feeds == nil then
        fail(where, "a rule needs a 'pattern', 'feeds', or both")
    end
    return { tag = tag, re = re, readers = readers, feeds = feeds, urls = urls, where = where }
end

-- Compiled here, so that a bad rule fails when the plugin loads rather than
-- on every entry.
local rules = {}
do
    local list = config.rules or {}
    if type(list) ~= "table" then
        fail("rules", "must be a list of rules")
    end
    for i, rule in ipairs(list) do
        table.insert(rules, compile_rule(string.format("rules[%d]", i), rule))
    end
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
    if rule.re == nil then
        return true
    end
    for _, read in ipairs(rule.readers) do
        for _, value in ipairs(read(entry)) do
            if type(value) == "string" and rule.re:is_match(value) then
                return true
            end
        end
    end
    return false
end

-- The tags of the rules `entry` matches, in rule order, without repeats.
local function tags_for(entry)
    local tags, seen = {}, {}
    for _, rule in ipairs(rules) do
        if not seen[rule.tag] and applies(rule, entry) and matches(rule, entry) then
            kiki.log("debug", string.format(
                "auto-tag: tagging %q %s: %s matched", entry.guid, rule.tag, rule.where))
            seen[rule.tag] = true
            table.insert(tags, rule.tag)
        end
    end
    return tags
end

kiki.on("entry.ingest", function(entry)
    for _, tag in ipairs(tags_for(entry)) do
        local present = false
        -- An earlier plugin may have added the tag already.
        for _, existing in ipairs(entry.tags) do
            if existing == tag then
                present = true
                break
            end
        end
        if not present then
            table.insert(entry.tags, tag)
        end
    end
    return entry
end)

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
-- that would tag again any entry the user untagged.
--
-- The rules are only recorded as applied once the scan has gone through
-- every entry. A scan cut short, by a reload or the server stopping, runs
-- again from the start on the next load.
kiki.on("plugin.load", function()
    if config.rescan == false then
        return
    end
    local applied = config.rules or {}
    if deep_equal(kiki.store.get("rules"), applied) then
        return
    end
    if #rules == 0 then
        kiki.store.set("rules", applied)
        return
    end
    kiki.log("info", "auto-tag: rules changed; applying them to stored entries")
    local tagged = 0
    kiki.entries.scan(function(entry)
        local added = false
        for _, tag in ipairs(tags_for(entry)) do
            if kiki.entries.tag(entry.id, tag) then
                added = true
            end
        end
        if added then
            tagged = tagged + 1
        end
    end, function(summary)
        kiki.log("info", string.format(
            "auto-tag: applied the rules to %d stored entries, tagging %d",
            summary.scanned, tagged))
        kiki.store.set("rules", applied)
    end)
end)
