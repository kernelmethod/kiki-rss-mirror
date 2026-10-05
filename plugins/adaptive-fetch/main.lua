-- Adaptive fetching: back off from feeds that keep not changing.
--
-- Some servers send a freshness hint far shorter than the feed's fetch
-- interval (`Cache-Control: max-age=0` is common), which on its own has
-- the feed fetched at the minimum polling cadence even if it changes once
-- a week. This plugin keeps one small counter per feed, its level:
--
-- - a fetch that finds the feed unchanged (a 304, or a 200 whose body is
--   the same as the last one) raises the level by one;
-- - a fetch that finds it changed lowers the level by one;
-- - a fetch that cannot tell (the feed's first) leaves it alone.
--
-- The wait the hint asks for is then stretched to
-- max(hint, min_cadence) * 2^level, never past the feed's own fetch
-- interval. Raising on no change and lowering on change settles the wait
-- near the feed's real update period: about where half the fetches find
-- something new.
--
-- Kiki only asks (with the fetch.schedule event) about feeds whose hint is
-- shorter than their interval; a feed without one already waits its full
-- interval, and its level is left as it was.
--
-- Config:
--
--   feeds    The feeds to back off from, each given by its id or by the
--            URL it is fetched from. Empty, every feed.
--   exclude  Feeds never to back off from, given the same way. Their
--            levels are dropped.
--
-- Levels are kept in the plugin's store, under `level:<feed id>`, so they
-- outlive restarts; a feed at level zero has no key.

local config = ...

local function fail(message)
    error("adaptive-fetch: " .. message, 0)
end

-- A list of feeds from the config, as a set of ids and a set of URLs.
local function feed_set(name)
    local ids, urls = {}, {}
    local list = config[name] or {}
    if type(list) ~= "table" then
        fail(string.format("'%s' must be a list of feed ids or URLs", name))
    end
    for i, feed in ipairs(list) do
        if type(feed) == "string" and feed ~= "" then
            urls[feed] = true
        else
            local id = type(feed) == "number" and math.tointeger(feed)
            if not id then
                fail(string.format("%s[%d] must be a feed id or URL", name, i))
            end
            ids[id] = true
        end
    end
    return { ids = ids, urls = urls, empty = next(ids) == nil and next(urls) == nil }
end

local only = feed_set("feeds")
local exclude = feed_set("exclude")

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
    return url
end

local function contains(set, feed_id)
    if set.ids[feed_id] then
        return true
    end
    if next(set.urls) == nil then
        return false
    end
    local url = feed_url(feed_id)
    return url and set.urls[url] == true or false
end

-- Whether the plugin backs off from feed `feed_id`.
local function applies(feed_id)
    if contains(exclude, feed_id) then
        return false
    end
    return only.empty or contains(only, feed_id)
end

-- Each feed's level, as read from or written to the store, by feed id.
-- The plugin is the only writer of its store, so once read a level is
-- kept here, and the store is only written when it changes.
local levels = {}

local function store_key(feed_id)
    return "level:" .. feed_id
end

local function level_of(feed_id)
    local level = levels[feed_id]
    if level == nil then
        local stored = kiki.store.get(store_key(feed_id))
        level = type(stored) == "number" and math.tointeger(stored) or 0
        if level < 0 then
            level = 0
        end
        levels[feed_id] = level
    end
    return level
end

local function set_level(feed_id, level)
    if level ~= level_of(feed_id) then
        levels[feed_id] = level
        kiki.store.set(store_key(feed_id), level > 0 and level or nil)
    end
end

-- The lowest level at which `base` doubled that many times reaches
-- `interval`. Levels above it change nothing, so the level is held there
-- to let a feed that starts changing come back down quickly.
local function max_level(base, interval)
    local level, wait = 0, base
    while wait < interval do
        wait = wait * 2
        level = level + 1
    end
    return level
end

-- `base` doubled `level` times, but no more than `interval`.
local function stretch(base, level, interval)
    local wait = base
    for _ = 1, level do
        wait = wait * 2
        if wait >= interval then
            return interval
        end
    end
    return wait
end

kiki.on("fetch.schedule", function(fetch)
    local feed_id = fetch.feed_id
    if not applies(feed_id) then
        set_level(feed_id, 0)
        return nil
    end

    local interval = fetch.interval_secs
    local base = math.max(fetch.hint_secs, fetch.min_cadence_secs, 1)
    if base >= interval then
        return nil
    end
    local top = max_level(base, interval)
    -- A level stored when the feed's interval was longer is brought back.
    local level = math.min(level_of(feed_id), top)
    if fetch.change == "unchanged" then
        level = math.min(level + 1, top)
    elseif fetch.change == "changed" then
        level = math.max(level - 1, 0)
    end
    set_level(feed_id, level)

    if level == 0 then
        return nil
    end
    -- Never shorten a wait another plugin has already lengthened.
    return math.max(stretch(base, level, interval), fetch.wait_secs)
end)

-- A removed feed's id may be given to a new feed.
kiki.on("feed.removed", function(feed)
    feed_urls[feed.id] = nil
    levels[feed.id] = 0
    kiki.store.set(store_key(feed.id), nil)
end)

-- A feed's URL changes when it is permanently redirected. Only watched for
-- when feeds are named by URL, since every handler costs each fetch a
-- little.
if next(only.urls) ~= nil or next(exclude.urls) ~= nil then
    kiki.on("fetch.success", function(fetch)
        feed_urls[fetch.feed_id] = nil
    end)
end
