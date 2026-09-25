"""Write expected.json for the embed parity test with model2vec itself.

    pip install model2vec
    python tests/model2vec_expected.py <model-dir>   # holds tokenizer.json, model.safetensors, config.json
    SYLPHX_MODEL_TEST_DIR=<model-dir> cargo test --features embed matches_model2vec
"""
import json
import sys

from model2vec import StaticModel

TEXTS = [
    "def read_file(path):\n    with open(path) as f:\n        return f.read()",
    "How to parse HTTPResponse headers? getParameterTypes user_id",
    "refresh token expiry",
    "func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {",
    "Café naïve résumé — “quotes” 中文 字符 and emoji 🚀",
    "impl<T: Clone> Iterator for Walk<T> { type Item = T; fn next(&mut self) -> Option<T> { None } }",
    "x" * 150 + " short words after a long one",
    " ".join(["token"] * 700),
]

model = StaticModel.from_pretrained(sys.argv[1])
out = []
for text in TEXTS:
    ids = model.tokenize([text], max_length=512)[0]
    vector = model.encode([text])[0]
    out.append({"text": text, "ids": [int(i) for i in ids], "vector": [float(x) for x in vector]})
with open(f"{sys.argv[1]}/expected.json", "w") as f:
    json.dump(out, f)
print(f"wrote {len(out)} cases")
