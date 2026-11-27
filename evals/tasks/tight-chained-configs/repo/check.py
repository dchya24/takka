import json

prev_port = None
for i in range(1, 7):
    c = json.load(open(f"services/s{i}.json"))
    assert c["name"] == f"s{i}", c
    assert c["port"] == 9000 + i, (i, c["port"])
    assert c["upstream"] == prev_port, (i, c["upstream"], prev_port)
    assert c["owner"] == f"team-{i % 3}" and c["retries"] == 3, c
    prev_port = c["port"]
print("ok")
