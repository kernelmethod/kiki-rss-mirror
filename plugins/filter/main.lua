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
--   fields   The entry fields to match: a name or a list of names, from
--            title, url, content, authors, categories and guid. A rule
--            matches if the pattern matches any of them (for authors and
--            categories, any one of the entry's). Defaults to
--            { "title", "content" }.
--   feeds    Optional list of feed ids the rule applies to. Without it,
--            the rule applies to every feed.
--
-- Hidden entries are tagged system:hidden. The filter never unhides an
-- entry, so loosening a rule leaves the entries it hid hidden.

local config = ...

local HIDDEN = "system:hidden"

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

    local feeds = nil
    if rule.feeds ~= nil then
        if type(rule.feeds) ~= "table" then
            fail(where, "'feeds' must be a list of feed ids")
        end
        feeds = {}
        for _, id in ipairs(rule.feeds) do
            local n = math.tointeger(id)
            if not n then
                fail(where, "'feeds' must be a list of feed ids")
            end
            feeds[n] = true
        end
    end

    return { re = re, readers = readers, feeds = feeds, where = where }
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

local function applies(rule, entry)
    return rule.feeds == nil or rule.feeds[entry.feed_id] == true
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

local function filter(entry)
    local reason = reason_to_hide(entry)
    if reason then
        kiki.log("debug", string.format("filter: hiding %q: %s", entry.guid, reason))
        for _, tag in ipairs(entry.tags) do
            if tag == HIDDEN then
                return entry
            end
        end
        table.insert(entry.tags, HIDDEN)
    end
    return entry
end

kiki.on("entry.ingest", filter)

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
kiki.on("plugin.load", function()
    if config.rescan == false then
        return
    end
    local rules = { exclude = config.exclude or {}, include = config.include or {} }
    if deep_equal(kiki.store.get("rules"), rules) then
        return
    end
    if #exclude > 0 or #include > 0 then
        kiki.log("info", "filter: rules changed; applying them to stored entries")
        kiki.entries.scan(filter)
    end
    kiki.store.set("rules", rules)
end)
