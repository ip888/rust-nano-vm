# Import the modules an agent tool call is most likely to reach for,
# so they're already resident in the snapshot's memory image.
python3 -c "import json, re, os, sys, itertools, functools, collections, dataclasses, typing"
python3 --version
