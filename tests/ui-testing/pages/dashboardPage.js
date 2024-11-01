import { expect } from '@playwright/test';
import { CommomnLocator } from '../pages/CommonLocator';
import { dateTimeButtonLocator, relative30SecondsButtonLocator, absoluteTabLocator, Past30SecondsValue } from '../pages/CommonLocator.js';


export class DashboardPage {
  constructor(page) {
    this.page = page;

    this.dashboardsMenuItem = page.locator('[data-test="menu-link-\\/dashboards-item"]');
    this.addDashboardButton = page.locator('[data-test="dashboard-add"]');
    this.dashboardNameInput = '[data-test="add-dashboard-name"]';
    this.dashboardSubmitButton = '[data-test="dashboard-add-submit"]';

    this.dateTimeButton = dateTimeButtonLocator;
    this.relative30SecondsButton = page.locator(relative30SecondsButtonLocator);
    this.absoluteTab = absoluteTabLocator;


    this.profileButton = page.locator('button').filter({ hasText: (process.env["ZO_ROOT_USER_EMAIL"]) });
    this.signOutButton = page.getByText('Sign Out');

  }

  async navigateToDashboards() {
    await this.dashboardsMenuItem.click();
    await this.page.waitForTimeout(5000);
  }

  async addDashboard(dashboardName) {
    await this.addDashboardButton.click();
    await this.page.waitForTimeout(5000);
    await expect(this.page.locator(this.dashboardNameInput)).toBeVisible();
    await this.page.locator(this.dashboardNameInput).fill(dashboardName);
    await expect(this.page.locator(this.dashboardSubmitButton)).toBeVisible();

    await this.page.locator(this.dashboardSubmitButton).click();
    await this.page.waitForTimeout(5000);
  }

  async setTimeToPast30Seconds() {
    // Set the time filter to the last 30 seconds
    await this.page.locator(this.dateTimeButton).click();
    await this.relative30SecondsButton.click();
  }

  async verifyTimeSetTo30Seconds() {
    // Verify that the time filter displays "Past 30 Seconds"
    await expect(this.page.locator(this.dateTimeButton)).toContainText(Past30SecondsValue);
  }

  async setDateTime() {
    await expect(this.page.locator(this.dateTimeButton)).toBeVisible();
    await this.page.locator(this.dateTimeButton).click();
    await this.page.locator(this.absoluteTab).click();
    await this.page.waitForTimeout(2000);

  }

  async fillTimeRange(startTime, endTime) {
    await this.page.getByRole('button', { name: '1', exact: true }).click();
    await this.page.getByLabel('access_time').first().fill(startTime);
    await this.page.getByRole('button', { name: '1', exact: true }).click();
    await this.page.getByLabel('access_time').nth(1).fill(endTime);

  }

  async verifyDateTime(startTime, endTime) {
    await expect(this.page.locator(this.dateTimeButton)).toContainText(`${startTime} - ${endTime}`);
  }

  async signOut() {
    await this.profileButton.click();
    await this.signOutButton.click();
  }

}

