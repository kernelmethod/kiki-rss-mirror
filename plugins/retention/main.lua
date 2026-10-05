-- Retention: delete entries some days after their feed stops listing them.
--
-- Kiki keeps an entry for as long as its feed lists it, and notes when a
-- refresh finds that the feed no longer does. This plugin deletes entries
-- whose feed stopped listing them more than `max_age_days` days ago, with
-- kiki.entries.delete_where, which never deletes an entry its feed still
-- lists: that entry would be fetched again, as new and unread.
--
-- It cleans up when plugins load, and then every hour.
--
-- Config:
--
--   max_age_days  How many days to keep an entry after its feed stops
--                 listing it. 0 keeps every entry forever.
--   keep_saved    Never delete entries tagged system:saved.

local config = ...

local function fail(message)
    error("retention: " .. message, 0)
end

local DAY = 24 * 60 * 60
local MAX_DAYS = 36500

local days = config.max_age_days or 0
if math.type(days) ~= "integer" or days < 0 or days > MAX_DAYS then
    fail(string.format("'max_age_days' must be a whole number of days from 0 to %d", MAX_DAYS))
end
local keep_saved = config.keep_saved
if keep_saved == nil then
    keep_saved = true
elseif type(keep_saved) ~= "boolean" then
    fail("'keep_saved' must be true or false")
end

local function clean_up()
    local deleted = kiki.entries.delete_where({
        dropped_before = os.time() - days * DAY,
        include_saved = not keep_saved,
    })
    if deleted > 0 then
        kiki.log("info", string.format("retention: deleted %d entries", deleted))
    end
end

if days > 0 then
    kiki.on("plugin.load", clean_up)
    kiki.every(60 * 60, clean_up)
end
