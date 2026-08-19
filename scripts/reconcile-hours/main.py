import sys
import requests as r

def main(args):
    if len(args) < 2:
        print("Usage: python main.py <CID> <YYYY-MM-DD>")
        return 1

    cid = -1
    try:
        cid = int(args[0])
        if cid <= 0:
            raise ValueError("CID must be positive")
    except ValueError:
        print("Invalid CID")
        return 1

    date = args[1]
    url = f"https://api.vatsim.net/api/ratings/{cid}/atcsessions/?start={date}"

    resp = r.get(url)
    if resp.status_code != 200:
        print(f"Failed to fetch data ({resp.status_code}): {resp.text}")
        return 1

    data = resp.json()

    total_mins = 0.0
    results = data["results"]
    for result in results:
        conn_id = result["connection_id"]
        minutes_on_callsign = float(result["minutes_on_callsign"])
        # start = result["start"]
        # end = result["end"]
        # total_minutes_on_callsign = result["total_minutes_on_callsign"]

        if minutes_on_callsign > 1440:
            print(f"Erroneous minutes_on_callsign for connection {conn_id}: {minutes_on_callsign}")
            continue

        total_mins += minutes_on_callsign

    print(f"Total minutes from {date} until now for {cid}: {total_mins}")

    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
