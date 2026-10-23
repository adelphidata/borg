---
name: stocks
description: "Look up market data with no account: current stock, ETF, index, currency, and crypto prices, day change, 52-week range, price history and return over a period, side-by-side comparisons, and ticker search by company name. Read-only; never places trades."
category: utilities
triggers:
  - "what's the stock price of"
  - "how is the market today"
  - "bitcoin price"
  - "compare these stocks"
  - "ticker for"
  - "how did this stock do this year"
requires:
  bins: ["curl", "jq"]
install:
  curl:
    brew: curl
    apt: curl
  jq:
    brew: jq
    apt: jq
---

# Stocks and crypto

Yahoo Finance's public chart endpoints, no key. They are unofficial: they can
change or rate-limit without notice. They need a browser-like User-Agent (the
default curl one gets a 429). Prices can lag by about 15 minutes; always show
the quote time. This is information, not financial advice.

```bash
UA="Mozilla/5.0 (Macintosh)"
```

Symbols: stocks `AAPL`, other exchanges with a suffix (`VOD.L`, `SAP.DE`,
`7203.T`), indexes `^GSPC` `^DJI` `^IXIC` `^FTSE`, currencies `EURUSD=X`,
crypto `BTC-USD` `ETH-USD`, futures `GC=F` (gold) `CL=F` (oil).

## Find a ticker

```bash
curl -s -G -A "$UA" https://query1.finance.yahoo.com/v1/finance/search \
  --data-urlencode "q=Nvidia" -d quotesCount=5 -d newsCount=0 |
  jq -r '.quotes[] | "\(.symbol)  \(.shortname // .longname)  \(.exchDisp // .exchange)  \(.quoteType)"'
```

## Quote (one or several)

```bash
for s in AAPL MSFT ^GSPC BTC-USD; do
  curl -s -A "$UA" "https://query1.finance.yahoo.com/v8/finance/chart/$s?range=1d&interval=1d" |
  jq -r '.chart | if .error then "\(.error.code): \(.error.description)" else .result[0].meta
    | (.regularMarketPrice - .chartPreviousClose) as $d
    | "\(.symbol)  \(.regularMarketPrice) \(.currency)  \(if $d >= 0 then "+" else "" end)\($d * 100 | round / 100) (\($d / .chartPreviousClose * 10000 | round / 100)%)  52w \(.fiftyTwoWeekLow) to \(.fiftyTwoWeekHigh)  at \(.regularMarketTime | todate)" end'
  sleep 0.5
done
```

Use `range=1d` for the day change: with a longer range, `chartPreviousClose`
is the close before that whole range.

## History and return

Ranges: `5d 1mo 3mo 6mo ytd 1y 2y 5y max`. Intervals: `1d`, `1wk`, `1mo`
(`5m` or `1h` for ranges up to a few days).

```bash
curl -s -A "$UA" "https://query1.finance.yahoo.com/v8/finance/chart/NVDA?range=6mo&interval=1d" |
jq -r '.chart.result[0] | [.timestamp, .indicators.quote[0].close] | transpose
  | map(select(.[1] != null)) | first as $f | last as $l
  | "\($f[0] | todate[:10]) \($f[1] * 100 | round / 100)  to  \($l[0] | todate[:10]) \($l[1] * 100 | round / 100)",
    "return \((($l[1] / $f[1]) - 1) * 10000 | round / 100)%   low \(map(.[1]) | min * 100 | round / 100)   high \(map(.[1]) | max * 100 | round / 100)"'
```

These closes are not adjusted for dividends. For a total return, use
`.indicators.adjclose[0].adjclose` instead of the close series.

To compare tickers, run the history line for each over the same range and
show them in one table: price, day change, period return, 52-week range.

## Crypto beyond Yahoo

CoinGecko covers more coins, no key (rate-limited):

```bash
curl -s "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin,ethereum,solana&vs_currencies=usd,eur&include_24hr_change=true" | jq .
curl -s "https://api.coingecko.com/api/v3/search?query=dogwifhat" | jq -r '.coins[:5][] | "\(.id)  \(.symbol)  \(.name)"'
```

## Price alerts

For "tell me if AAPL drops below 300", create a `schedule` prompt job (for
example hourly in market hours: `"0 14-21 * * 1-5"` in UTC for New York) that
runs the quote and alerts only when the condition holds. The `price-watch`
skill shows how to keep state so the same alert is not sent twice.
