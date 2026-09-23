-- Phase B payroll (PAY-13): a FIRST salary applies from the start.
--
-- The salary-history trigger dates every change CURRENT_DATE, which is right
-- for a raise. But a person added with no salary (or 0) and given one later
-- would have every earlier day of the month priced at 0: base pay
-- pro-rated to the days since the salary was typed. Nothing was ever paid at
-- the old figure, so the first real salary is the salary from the hire date
-- — the history is rewritten to that one row.
CREATE OR REPLACE FUNCTION employees_salary_history_trg() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
        VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, COALESCE(NEW.hire_date, CURRENT_DATE))
        ON CONFLICT (employee_id, effective_from) DO UPDATE SET base_salary_piastres = EXCLUDED.base_salary_piastres;
    ELSIF NEW.base_salary_piastres IS DISTINCT FROM OLD.base_salary_piastres THEN
        IF COALESCE(OLD.base_salary_piastres, 0) = 0 THEN
            DELETE FROM employee_salary_history WHERE employee_id = NEW.id;
            INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
            VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, COALESCE(NEW.hire_date, CURRENT_DATE));
        ELSE
            INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
            VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, CURRENT_DATE)
            ON CONFLICT (employee_id, effective_from) DO UPDATE SET base_salary_piastres = EXCLUDED.base_salary_piastres;
        END IF;
    END IF;
    RETURN NEW;
END $$;
