# Python 3.12 + the data-science stack. Target for agent tool calls
# that need to slice DataFrames + do quick modeling without paying
# `pip install` cold-start on every fork.
FROM python:3.12-slim@sha256:1e5d0da4ae4f8fdd9c6b70d4a7c62d5236b23b3a3f65f7dab52d6c7ea3a4e79c
RUN pip install --no-cache-dir \
    pandas==2.2.3 \
    numpy==2.0.2 \
    scikit-learn==1.5.2 \
    matplotlib==3.9.2
CMD ["python3"]
