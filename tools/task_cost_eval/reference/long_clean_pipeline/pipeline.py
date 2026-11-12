"""Clean a raw CSV: python3 pipeline.py RAW_CSV OUT_DIR writes clean.csv and rejects.csv."""
import csv
import importlib
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
COLUMNS = ('orders_phone', 'invoices_phone', 'refunds_phone', 'carts_phone', 'coupons_phone', 'shipments_date', 'returns_date', 'vendors_date', 'payouts_date', 'ledgers_date', 'budgets_amount', 'quotes_amount', 'receipts_amount', 'tickets_amount', 'alerts_amount', 'reports_email', 'sessions_email', 'tokens_email', 'teams_email', 'members_email', 'projects_country', 'tasks_country', 'comments_country', 'labels_country', 'files_country', 'folders_postcode', 'uploads_postcode', 'exports_postcode', 'imports_postcode', 'schedules_postcode', 'reminders_name', 'events_name', 'venues_name', 'bookings_name', 'rooms_name', 'guests_flag', 'menus_flag', 'dishes_flag', 'stocks_flag', 'batches_flag')


def clean_row(row):
    """Normalized row and the names of columns whose value is invalid."""
    cleaned, invalid = {}, []
    for column in COLUMNS:
        value = importlib.import_module(f'normalizers.{column}').normalize(row[column])
        cleaned[column] = value
        if value is None:
            invalid.append(column)
    return cleaned, invalid


def run(raw_path, out_dir):
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    with open(raw_path, newline='') as handle:
        rows = list(csv.DictReader(handle))
    with open(out_dir / 'clean.csv', 'w', newline='') as clean, open(out_dir / 'rejects.csv', 'w', newline='') as rejects:
        good = csv.DictWriter(clean, ['row', *COLUMNS])
        bad = csv.writer(rejects)
        good.writeheader()
        bad.writerow(['row', 'columns'])
        for number, row in enumerate(rows, 1):
            cleaned, invalid = clean_row(row)
            if invalid:
                bad.writerow([number, ' '.join(invalid)])
            else:
                good.writerow({'row': number, **cleaned})


if __name__ == '__main__':
    if len(sys.argv) != 3:
        print('usage: python3 pipeline.py RAW_CSV OUT_DIR', file=sys.stderr)
        raise SystemExit(2)
    run(sys.argv[1], sys.argv[2])
