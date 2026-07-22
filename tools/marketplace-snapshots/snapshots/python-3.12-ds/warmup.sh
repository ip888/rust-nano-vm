# The DS stack takes ~2s to import on cold start (numpy's C ABI +
# pandas' extension init). Pre-import so the snapshot's memory
# image holds the parsed bytecode + loaded shared libs.
python3 -c "
import pandas as pd, numpy as np
from sklearn.linear_model import LinearRegression
import matplotlib
matplotlib.use('Agg')   # headless
import matplotlib.pyplot as plt

# Touch each library so the import side effects (numpy C-ext init,
# pandas' arrow bridge, sklearn's compiled kernels) all land.
df = pd.DataFrame({'x': np.arange(4), 'y': np.arange(4) * 2})
LinearRegression().fit(df[['x']], df['y'])
plt.figure()
print('warm:', pd.__version__, np.__version__)
"
