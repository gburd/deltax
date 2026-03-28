"""All 43 official ClickBench queries for pg_deltax benchmark.

Queries taken verbatim from the ClickBench postgresql/queries.sql reference.
Each entry: (query_id, short_description, sql_text).

Query IDs are 0-indexed (Q0..Q42) to match the ClickBench reporting convention.

Note: EventDate references are kept as-is (DATE type in schema).
EventTime is TIMESTAMPTZ in our schema; queries referencing it work unchanged.
"""

# Queries with non-deterministic result ordering due to ties in ORDER BY
# with LIMIT/OFFSET, or no ORDER BY at all.  Content comparison between
# compressed and uncompressed scans is not meaningful for these.
NONDETERMINISTIC_QUERIES = {
    "Q17",  # GROUP BY + LIMIT, no ORDER BY
    "Q24",  # ORDER BY EventTime LIMIT — timestamp ties (single sort key not in SELECT)
    "Q26",  # ORDER BY EventTime, SearchPhrase LIMIT — primary sort key not in SELECT
}

# Validation hints for non-deterministic queries.
# Maps qid -> (sort_key_col_index, "ASC"|"DESC", has_offset).
# None means no ORDER BY (only row count can be validated).
NONDET_SORT_INFO = {
    "Q17": None,                 # no ORDER BY
    "Q24": None,                 # ORDER BY EventTime, but not in SELECT list
    "Q26": None,                 # ORDER BY EventTime, SearchPhrase — not all sort cols in SELECT
}

# Queries where ORDER BY ... LIMIT can have ties at the boundary.
# Maps qid -> 0-based column index of the sort key.
# Validation: strip rows sharing the last-row's sort key, then exact-match the rest.
# For OFFSET queries, ties at both ends of the window are stripped.
LIMIT_TIE_QUERIES = {
    "Q9": 2,   # COUNT(*) AS c
    "Q11": 2,  # COUNT(DISTINCT UserID) AS u
    "Q18": 3,  # COUNT(*)
    "Q22": 3,  # COUNT(*) AS c
    "Q23": 4,  # EventTime (col 4 in SELECT *)
    "Q30": 2,  # COUNT(*) AS c
    "Q31": 2,  # COUNT(*) AS c
    "Q32": 2,  # COUNT(*) AS c
    "Q38": 1,  # PageViews (OFFSET 1000)
    "Q39": 5,  # PageViews (OFFSET 1000)
    "Q40": 2,  # PageViews (OFFSET 100)
}

QUERIES = [
    # Q0: COUNT(*) — full scan baseline
    (
        "Q0",
        "COUNT(*)",
        "SELECT COUNT(*) FROM hits",
    ),
    # Q1: Filtered count
    (
        "Q1",
        "COUNT WHERE AdvEngineID",
        "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0",
    ),
    # Q2: SUM/AVG aggregation — full scan
    (
        "Q2",
        "SUM/AVG full scan",
        "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits",
    ),
    # Q3: AVG UserID
    (
        "Q3",
        "AVG UserID",
        "SELECT AVG(UserID) FROM hits",
    ),
    # Q4: COUNT DISTINCT UserID
    (
        "Q4",
        "COUNT DISTINCT UserID",
        "SELECT COUNT(DISTINCT UserID) FROM hits",
    ),
    # Q5: COUNT DISTINCT SearchPhrase
    (
        "Q5",
        "COUNT DISTINCT SearchPhrase",
        "SELECT COUNT(DISTINCT SearchPhrase) FROM hits",
    ),
    # Q6: MIN/MAX EventDate
    (
        "Q6",
        "MIN/MAX EventDate",
        "SELECT MIN(EventDate), MAX(EventDate) FROM hits",
    ),
    # Q7: GROUP BY AdvEngineID
    (
        "Q7",
        "GROUP BY AdvEngineID",
        "SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 "
        "GROUP BY AdvEngineID ORDER BY COUNT(*) DESC",
    ),
    # Q8: GROUP BY RegionID with DISTINCT
    (
        "Q8",
        "GROUP BY RegionID",
        "SELECT RegionID, COUNT(DISTINCT UserID) AS u FROM hits "
        "GROUP BY RegionID ORDER BY u DESC LIMIT 10",
    ),
    # Q9: GROUP BY RegionID multi-agg
    (
        "Q9",
        "RegionID multi-agg",
        "SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), "
        "COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10",
    ),
    # Q10: MobilePhoneModel distinct users
    (
        "Q10",
        "MobilePhoneModel users",
        "SELECT MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits "
        "WHERE MobilePhoneModel <> '' GROUP BY MobilePhoneModel ORDER BY u DESC LIMIT 10",
    ),
    # Q11: MobilePhone + Model distinct users
    (
        "Q11",
        "MobilePhone+Model users",
        "SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits "
        "WHERE MobilePhoneModel <> '' GROUP BY MobilePhone, MobilePhoneModel "
        "ORDER BY u DESC LIMIT 10",
    ),
    # Q12: Top SearchPhrase
    (
        "Q12",
        "Top SearchPhrase",
        "SELECT SearchPhrase, COUNT(*) AS c FROM hits "
        "WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
    ),
    # Q13: SearchPhrase distinct users
    (
        "Q13",
        "SearchPhrase users",
        "SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits "
        "WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY u DESC LIMIT 10",
    ),
    # Q14: SearchEngineID + SearchPhrase
    (
        "Q14",
        "SearchEngine+Phrase",
        "SELECT SearchEngineID, SearchPhrase, COUNT(*) AS c FROM hits "
        "WHERE SearchPhrase <> '' GROUP BY SearchEngineID, SearchPhrase "
        "ORDER BY c DESC LIMIT 10",
    ),
    # Q15: Top UserID
    (
        "Q15",
        "Top UserID",
        "SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10",
    ),
    # Q16: UserID + SearchPhrase ordered
    (
        "Q16",
        "UserID+SearchPhrase top",
        "SELECT UserID, SearchPhrase, COUNT(*) FROM hits "
        "GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10",
    ),
    # Q17: UserID + SearchPhrase unordered
    (
        "Q17",
        "UserID+SearchPhrase",
        "SELECT UserID, SearchPhrase, COUNT(*) FROM hits "
        "GROUP BY UserID, SearchPhrase LIMIT 10",
    ),
    # Q18: UserID + minute + SearchPhrase
    (
        "Q18",
        "UserID+minute+Phrase",
        "SELECT UserID, extract(minute FROM EventTime) AS m, SearchPhrase, COUNT(*) "
        "FROM hits GROUP BY UserID, m, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10",
    ),
    # Q19: Point lookup by UserID
    (
        "Q19",
        "Point lookup UserID",
        "SELECT UserID FROM hits WHERE UserID = 435090932899640449",
    ),
    # Q20: URL LIKE google
    (
        "Q20",
        "URL LIKE google",
        "SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'",
    ),
    # Q21: SearchPhrase + URL LIKE google
    (
        "Q21",
        "SearchPhrase+URL google",
        "SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits "
        "WHERE URL LIKE '%google%' AND SearchPhrase <> '' "
        "GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
    ),
    # Q22: Title LIKE Google complex
    (
        "Q22",
        "Title LIKE Google",
        "SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, "
        "COUNT(DISTINCT UserID) FROM hits "
        "WHERE Title LIKE '%Google%' AND URL NOT LIKE '%.google.%' "
        "AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
    ),
    # Q23: SELECT * URL LIKE google ORDER BY EventTime
    (
        "Q23",
        "SELECT * google sorted",
        "SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10",
    ),
    # Q24: SearchPhrase ORDER BY EventTime
    (
        "Q24",
        "SearchPhrase by time",
        "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' "
        "ORDER BY EventTime LIMIT 10",
    ),
    # Q25: SearchPhrase ORDER BY SearchPhrase
    (
        "Q25",
        "SearchPhrase sorted",
        "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' "
        "ORDER BY SearchPhrase LIMIT 10",
    ),
    # Q26: SearchPhrase ORDER BY EventTime, SearchPhrase
    (
        "Q26",
        "SearchPhrase time+phrase",
        "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' "
        "ORDER BY EventTime, SearchPhrase LIMIT 10",
    ),
    # Q27: CounterID avg URL length
    (
        "Q27",
        "CounterID avg URL len",
        "SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits "
        "WHERE URL <> '' GROUP BY CounterID HAVING COUNT(*) > 100000 "
        "ORDER BY l DESC LIMIT 25",
    ),
    # Q28: Referer domain extraction
    (
        "Q28",
        "Referer domain regex",
        r"SELECT REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1') AS k, "
        "AVG(length(Referer)) AS l, COUNT(*) AS c, MIN(Referer) FROM hits "
        "WHERE Referer <> '' GROUP BY k HAVING COUNT(*) > 100000 "
        "ORDER BY l DESC LIMIT 25",
    ),
    # Q29: Wide SUM aggregation (89 columns)
    (
        "Q29",
        "Wide SUM 89 cols",
        "SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), SUM(ResolutionWidth + 2), "
        "SUM(ResolutionWidth + 3), SUM(ResolutionWidth + 4), SUM(ResolutionWidth + 5), "
        "SUM(ResolutionWidth + 6), SUM(ResolutionWidth + 7), SUM(ResolutionWidth + 8), "
        "SUM(ResolutionWidth + 9), SUM(ResolutionWidth + 10), SUM(ResolutionWidth + 11), "
        "SUM(ResolutionWidth + 12), SUM(ResolutionWidth + 13), SUM(ResolutionWidth + 14), "
        "SUM(ResolutionWidth + 15), SUM(ResolutionWidth + 16), SUM(ResolutionWidth + 17), "
        "SUM(ResolutionWidth + 18), SUM(ResolutionWidth + 19), SUM(ResolutionWidth + 20), "
        "SUM(ResolutionWidth + 21), SUM(ResolutionWidth + 22), SUM(ResolutionWidth + 23), "
        "SUM(ResolutionWidth + 24), SUM(ResolutionWidth + 25), SUM(ResolutionWidth + 26), "
        "SUM(ResolutionWidth + 27), SUM(ResolutionWidth + 28), SUM(ResolutionWidth + 29), "
        "SUM(ResolutionWidth + 30), SUM(ResolutionWidth + 31), SUM(ResolutionWidth + 32), "
        "SUM(ResolutionWidth + 33), SUM(ResolutionWidth + 34), SUM(ResolutionWidth + 35), "
        "SUM(ResolutionWidth + 36), SUM(ResolutionWidth + 37), SUM(ResolutionWidth + 38), "
        "SUM(ResolutionWidth + 39), SUM(ResolutionWidth + 40), SUM(ResolutionWidth + 41), "
        "SUM(ResolutionWidth + 42), SUM(ResolutionWidth + 43), SUM(ResolutionWidth + 44), "
        "SUM(ResolutionWidth + 45), SUM(ResolutionWidth + 46), SUM(ResolutionWidth + 47), "
        "SUM(ResolutionWidth + 48), SUM(ResolutionWidth + 49), SUM(ResolutionWidth + 50), "
        "SUM(ResolutionWidth + 51), SUM(ResolutionWidth + 52), SUM(ResolutionWidth + 53), "
        "SUM(ResolutionWidth + 54), SUM(ResolutionWidth + 55), SUM(ResolutionWidth + 56), "
        "SUM(ResolutionWidth + 57), SUM(ResolutionWidth + 58), SUM(ResolutionWidth + 59), "
        "SUM(ResolutionWidth + 60), SUM(ResolutionWidth + 61), SUM(ResolutionWidth + 62), "
        "SUM(ResolutionWidth + 63), SUM(ResolutionWidth + 64), SUM(ResolutionWidth + 65), "
        "SUM(ResolutionWidth + 66), SUM(ResolutionWidth + 67), SUM(ResolutionWidth + 68), "
        "SUM(ResolutionWidth + 69), SUM(ResolutionWidth + 70), SUM(ResolutionWidth + 71), "
        "SUM(ResolutionWidth + 72), SUM(ResolutionWidth + 73), SUM(ResolutionWidth + 74), "
        "SUM(ResolutionWidth + 75), SUM(ResolutionWidth + 76), SUM(ResolutionWidth + 77), "
        "SUM(ResolutionWidth + 78), SUM(ResolutionWidth + 79), SUM(ResolutionWidth + 80), "
        "SUM(ResolutionWidth + 81), SUM(ResolutionWidth + 82), SUM(ResolutionWidth + 83), "
        "SUM(ResolutionWidth + 84), SUM(ResolutionWidth + 85), SUM(ResolutionWidth + 86), "
        "SUM(ResolutionWidth + 87), SUM(ResolutionWidth + 88), SUM(ResolutionWidth + 89) "
        "FROM hits",
    ),
    # Q30: SearchEngineID + ClientIP aggregation
    (
        "Q30",
        "SearchEngine+ClientIP",
        "SELECT SearchEngineID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), "
        "AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' "
        "GROUP BY SearchEngineID, ClientIP ORDER BY c DESC LIMIT 10",
    ),
    # Q31: WatchID + ClientIP (SearchPhrase filter)
    (
        "Q31",
        "WatchID+ClientIP filter",
        "SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), "
        "AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' "
        "GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10",
    ),
    # Q32: WatchID + ClientIP (no filter)
    (
        "Q32",
        "WatchID+ClientIP all",
        "SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), "
        "AVG(ResolutionWidth) FROM hits "
        "GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10",
    ),
    # Q33: Top URLs
    (
        "Q33",
        "Top URLs",
        "SELECT URL, COUNT(*) AS c FROM hits GROUP BY URL ORDER BY c DESC LIMIT 10",
    ),
    # Q34: Top URLs with constant
    (
        "Q34",
        "Top URLs with const",
        "SELECT 1, URL, COUNT(*) AS c FROM hits "
        "GROUP BY 1, URL ORDER BY c DESC LIMIT 10",
    ),
    # Q35: ClientIP arithmetic GROUP BY
    (
        "Q35",
        "ClientIP arithmetic",
        "SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c "
        "FROM hits GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3 "
        "ORDER BY c DESC LIMIT 10",
    ),
    # Q36: CounterID=62 URLs
    (
        "Q36",
        "CounterID=62 URLs",
        "SELECT URL, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' "
        "AND DontCountHits = 0 AND IsRefresh = 0 AND URL <> '' "
        "GROUP BY URL ORDER BY PageViews DESC LIMIT 10",
    ),
    # Q37: CounterID=62 Titles
    (
        "Q37",
        "CounterID=62 Titles",
        "SELECT Title, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' "
        "AND DontCountHits = 0 AND IsRefresh = 0 AND Title <> '' "
        "GROUP BY Title ORDER BY PageViews DESC LIMIT 10",
    ),
    # Q38: CounterID=62 URLs with link filter
    (
        "Q38",
        "CounterID=62 links",
        "SELECT URL, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' "
        "AND IsRefresh = 0 AND IsLink <> 0 AND IsDownload = 0 "
        "GROUP BY URL ORDER BY PageViews DESC LIMIT 10 OFFSET 1000",
    ),
    # Q39: CounterID=62 traffic sources
    (
        "Q39",
        "CounterID=62 traffic src",
        "SELECT TraficSourceID, SearchEngineID, AdvEngineID, "
        "CASE WHEN (SearchEngineID = 0 AND AdvEngineID = 0) THEN Referer ELSE '' END AS Src, "
        "URL AS Dst, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' "
        "AND IsRefresh = 0 "
        "GROUP BY TraficSourceID, SearchEngineID, AdvEngineID, Src, Dst "
        "ORDER BY PageViews DESC LIMIT 10 OFFSET 1000",
    ),
    # Q40: CounterID=62 URLHash + EventDate
    (
        "Q40",
        "CounterID=62 URLHash",
        "SELECT URLHash, EventDate, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' "
        "AND IsRefresh = 0 AND TraficSourceID IN (-1, 6) "
        "AND RefererHash = 3594120000172545465 "
        "GROUP BY URLHash, EventDate ORDER BY PageViews DESC LIMIT 10 OFFSET 100",
    ),
    # Q41: CounterID=62 WindowClient dimensions
    (
        "Q41",
        "CounterID=62 window dim",
        "SELECT WindowClientWidth, WindowClientHeight, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' "
        "AND IsRefresh = 0 AND DontCountHits = 0 "
        "AND URLHash = 2868770270353813622 "
        "GROUP BY WindowClientWidth, WindowClientHeight "
        "ORDER BY PageViews DESC LIMIT 10 OFFSET 10000",
    ),
    # Q42: CounterID=62 by minute
    (
        "Q42",
        "CounterID=62 by minute",
        "SELECT DATE_TRUNC('minute', EventTime) AS M, COUNT(*) AS PageViews FROM hits "
        "WHERE CounterID = 62 AND EventDate >= '2013-07-14' AND EventDate <= '2013-07-15' "
        "AND IsRefresh = 0 AND DontCountHits = 0 "
        "GROUP BY DATE_TRUNC('minute', EventTime) "
        "ORDER BY DATE_TRUNC('minute', EventTime) LIMIT 10 OFFSET 1000",
    ),
]
